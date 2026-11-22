//! `ClipboardProvider` (C3) on the Windows clipboard (F-CLIP-01, PROTOCOL.md 7.5): UTF-8 text through
//! `CF_UNICODETEXT` and PNG through the registered `"PNG"` format, with `CF_DIB` (screenshots, "copy image") converted to
//! a compressed PNG on read (the deflate of [`crate::deflate`], after the clipboard is closed). Content a password manager marked as hidden (`ExcludeClipboardContentFromMonitorProcessing`,
//! `CanIncludeInClipboardHistory` = 0, `Clipboard Viewer Ignore`) reads as `Concealed` and is never returned.
//!
//! Change detection is the clipboard sequence number (`GetClipboardSequenceNumber`), which the contract polls: no
//! window or format listener is needed. `OpenClipboard` fails while another program holds the clipboard, so it is
//! retried 10 times, 15 ms apart.
//!
//! Limits: the text and PNG limits of 7.5 are checked by the sync engine from the length of what is read; this module
//! only refuses to copy out more than 64 MiB (reported as `Unsupported`) and refuses to write an item over the limits.
//! Writing a PNG puts the `"PNG"` format on the clipboard only (no `CF_DIB`: that would need a PNG decoder); programs
//! that read `"PNG"` (browsers, Office, Paint) paste it.
//!
//! Line ends: text is read as the clipboard holds it (CR-LF) and written with CR-LF, a lone LF from a Mac peer
//! becoming CR-LF ([`windows_line_ends`]).
//!
//! The pure parts (DIB to PNG, the PNG length, UTF-16 decoding, the line ends, the concealed rule) build on every host.

use swapdesk_proto::MAX_CLIPBOARD_TEXT;

/// The most this module copies out of the clipboard for one format.
const COPY_CAP: usize = 64 * 1024 * 1024;

// ---------- pure parts ----------

/// The concealed rule: true when a password manager asked for the content to stay out of history and sync.
/// `history` is the DWORD of `CanIncludeInClipboardHistory` when the format is present.
pub(crate) fn is_concealed(exclude_monitor: bool, history: Option<u32>, viewer_ignore: bool) -> bool {
    exclude_monitor || viewer_ignore || history == Some(0)
}

/// Decodes `CF_UNICODETEXT` bytes (UTF-16LE, NUL ended, possibly padded by `GlobalSize`) into a string, stopping at
/// the NUL and once the text passes the 7.5 limit (the sync engine then reports it too large).
pub(crate) fn decode_text(bytes: &[u8]) -> String {
    let units = bytes.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).take_while(|&u| u != 0);
    let mut out = String::new();
    for ch in char::decode_utf16(units) {
        out.push(ch.unwrap_or(char::REPLACEMENT_CHARACTER));
        if out.len() > MAX_CLIPBOARD_TEXT as usize {
            break;
        }
    }
    out
}

/// The text a peer sent with the line ends `CF_UNICODETEXT` asks for ("Each line ends with a carriage
/// return/linefeed (CR-LF) combination", Microsoft Learn "Standard Clipboard Formats", read on 22/11/2026): a lone LF
/// becomes CR-LF. The conversion is made here, where text is written, and never where it is read: a Windows side
/// keeps sending its CR-LF as it is, so a 1.0 or 1.1.0 Windows peer, which writes what it gets as it comes, still
/// receives CR-LF; a Mac peer turns CR-LF into LF where it writes.
pub(crate) fn windows_line_ends(text: &str) -> std::borrow::Cow<'_, str> {
    let bytes = text.as_bytes();
    let lone = |i: usize| bytes[i] == b'\n' && (i == 0 || bytes[i - 1] != b'\r');
    let count = (0..bytes.len()).filter(|&i| lone(i)).count();
    if count == 0 {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + count);
    let mut from = 0;
    for i in (0..bytes.len()).filter(|&i| lone(i)) {
        // `i` is the byte of an ASCII LF: a character boundary.
        out.push_str(&text[from..i]);
        out.push_str("\r\n");
        from = i + 1;
    }
    out.push_str(&text[from..]);
    std::borrow::Cow::Owned(out)
}

pub(crate) fn encode_text(text: &str) -> Vec<u8> {
    text.encode_utf16().chain(std::iter::once(0)).flat_map(u16::to_le_bytes).collect()
}

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// The real length of a PNG held in a buffer that may be longer (a global memory block is rounded up): up to the end
/// of its `IEND` chunk. `None` when it is not a well-formed chunk sequence.
pub(crate) fn png_exact_len(data: &[u8]) -> Option<usize> {
    if data.get(..8)? != PNG_SIGNATURE {
        return None;
    }
    let mut at = 8usize;
    loop {
        let len = u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?) as usize;
        let kind = data.get(at + 4..at + 8)?;
        let end = at.checked_add(12)?.checked_add(len)?;
        if end > data.len() {
            return None;
        }
        if kind == b"IEND" {
            return Some(end);
        }
        at = end;
    }
}

/// The CRC-32 table of PNG (polynomial 0xEDB88320), built at compile time.
const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
            k += 1;
        }
        table[n] = c;
        n += 1;
    }
    table
};

fn crc32(data: &[&[u8]]) -> u32 {
    let mut crc = u32::MAX;
    for part in data {
        for &b in *part {
            crc = CRC_TABLE[((crc ^ u32::from(b)) & 0xFF) as usize] ^ (crc >> 8);
        }
    }
    !crc
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(&[kind, data]).to_be_bytes());
}

/// The Paeth predictor of PNG.
fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = i16::from(a) + i16::from(b) - i16::from(c);
    let (pa, pb, pc) = ((p - i16::from(a)).abs(), (p - i16::from(b)).abs(), (p - i16::from(c)).abs());
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// Appends `row` filtered for the compressor: of None, Sub, Up and Paeth, the one whose bytes, read as signed, sum to
/// the least (the heuristic of the PNG specification, 12.8). `prior` is the row above (zeros for the first).
fn filter_row(row: &[u8], prior: &[u8], scratch: &mut [Vec<u8>; 4], raw: &mut Vec<u8>) {
    const BPP: usize = 3;
    for (kind, out) in scratch.iter_mut().enumerate() {
        out.clear();
        out.extend(row.iter().enumerate().map(|(i, &x)| {
            let a = if i >= BPP { row[i - BPP] } else { 0 };
            let (b, c) = (prior[i], if i >= BPP { prior[i - BPP] } else { 0 });
            x.wrapping_sub(match kind {
                0 => 0,
                1 => a,
                2 => b,
                _ => paeth(a, b, c),
            })
        }));
    }
    let cost = |v: &Vec<u8>| v.iter().map(|&x| u64::from((x as i8).unsigned_abs())).sum::<u64>();
    let best = (0..4).min_by_key(|&k| cost(&scratch[k])).unwrap_or(0);
    raw.push([0, 1, 2, 4][best]);
    raw.extend_from_slice(&scratch[best]);
}

/// A PNG of 8-bit RGB rows, each row filtered and the whole compressed with deflate (review of 22/11/2026, D8: stored
/// blocks took a 4K screenshot past the 16 MiB limit of 7.5, which the sync engine checks).
pub(crate) fn encode_png_rgb(width: u32, height: u32, rgb_rows: &[u8]) -> Vec<u8> {
    let stride = width as usize * 3;
    let mut raw = Vec::with_capacity((stride + 1) * height as usize);
    let mut scratch: [Vec<u8>; 4] = Default::default();
    let zeros = vec![0u8; stride];
    let mut prior: &[u8] = &zeros;
    for row in rgb_rows.chunks_exact(stride) {
        filter_row(row, prior, &mut scratch, &mut raw);
        prior = row;
    }
    let z = crate::deflate::zlib(&raw);
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8 bit, truecolor, deflate, no filter, no interlace
    let mut out = PNG_SIGNATURE.to_vec();
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

/// Converts a `CF_DIB` block (a `BITMAPINFOHEADER` or later header, 24 or 32 bits, `BI_RGB`, or `BI_BITFIELDS` with
/// the usual BGRA masks) to a PNG. The alpha byte of a DIB is unreliable, so the PNG is opaque. `None` for anything
/// else (palettes, 16 bits, RLE, JPEG, huge sizes).
pub(crate) fn dib_to_png(dib: &[u8]) -> Option<Vec<u8>> {
    let le32 = |at: usize| dib.get(at..at + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
    let header = le32(0)? as usize;
    if header < 40 {
        return None;
    }
    let (width, height) = (le32(4)? as i32, le32(8)? as i32);
    let planes = u16::from_le_bytes(dib.get(12..14)?.try_into().ok()?);
    let bits = u16::from_le_bytes(dib.get(14..16)?.try_into().ok()?);
    let compression = le32(16)?;
    let colors_used = le32(32)? as usize;
    if planes != 1 || !matches!(bits, 24 | 32) || width <= 0 || height == 0 || height == i32::MIN {
        return None;
    }
    let mut offset = header.checked_add(colors_used.checked_mul(4)?)?;
    match compression {
        0 => {}
        3 if bits == 32 => {
            // The masks follow a 40-byte header; later headers carry them inside.
            if header == 40 {
                offset = offset.checked_add(12)?;
            }
            let (r, g, b) = (le32(40)?, le32(44)?, le32(48)?);
            if (r, g, b) != (0x00FF_0000, 0x0000_FF00, 0x0000_00FF) {
                return None;
            }
        }
        _ => return None,
    }
    let (w, h) = (width as usize, height.unsigned_abs() as usize);
    if w.checked_mul(h)?.checked_mul(3)? > COPY_CAP {
        return None;
    }
    let bytes_per_px = usize::from(bits / 8);
    let stride = (w * usize::from(bits)).div_ceil(32) * 4;
    let pixels = dib.get(offset..offset.checked_add(stride.checked_mul(h)?)?)?;
    let mut rgb = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        let src_row = if height > 0 { h - 1 - y } else { y };
        let row = &pixels[src_row * stride..src_row * stride + w * bytes_per_px];
        for px in row.chunks_exact(bytes_per_px) {
            rgb.extend_from_slice(&[px[2], px[1], px[0]]);
        }
    }
    Some(encode_png_rgb(width as u32, h as u32, &rgb))
}

#[cfg(windows)]
mod win {
    use std::time::Duration;

    use swapdesk_platform::{ClipContent, ClipRead, ClipboardProvider, ErrorKind, PlatformError, Result};
    use swapdesk_proto::{MAX_CLIPBOARD_PNG, MAX_CLIPBOARD_TEXT};
    use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, CountClipboardFormats, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
        IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock};
    use windows::Win32::System::Ole::{CF_DIB, CF_UNICODETEXT};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, HWND_MESSAGE, WINDOW_EX_STYLE, WINDOW_STYLE,
    };
    use windows::core::{PCWSTR, w};

    use super::{COPY_CAP, decode_text, dib_to_png, encode_text, is_concealed, png_exact_len, windows_line_ends};

    const OPEN_ATTEMPTS: u32 = 10;
    const OPEN_WAIT: Duration = Duration::from_millis(15);

    fn os_err(op: &'static str, e: &windows::core::Error) -> PlatformError {
        PlatformError { kind: ErrorKind::Os, op, os_code: Some(i64::from(e.code().0)), detail: String::new() }
    }

    fn register(name: PCWSTR) -> u32 {
        // SAFETY: `name` is a NUL-terminated wide string literal.
        unsafe { RegisterClipboardFormatW(name) }
    }

    /// The clipboard, open; closed on drop.
    struct Open;

    impl Open {
        fn with_retry(owner: Option<HWND>) -> Result<Self> {
            let mut last = None;
            for attempt in 0..OPEN_ATTEMPTS {
                // SAFETY: plain call; `owner` is a window of this thread or none.
                match unsafe { OpenClipboard(owner) } {
                    Ok(()) => return Ok(Self),
                    Err(e) => last = Some(e),
                }
                if attempt + 1 < OPEN_ATTEMPTS {
                    std::thread::sleep(OPEN_WAIT);
                }
            }
            let e = last.expect("at least one attempt");
            Err(PlatformError {
                kind: ErrorKind::Unavailable,
                op: "OpenClipboard",
                os_code: Some(i64::from(e.code().0)),
                detail: "held by another program".into(),
            })
        }
    }

    impl Drop for Open {
        fn drop(&mut self) {
            // SAFETY: this value exists only while this thread holds the clipboard open.
            let _ = unsafe { CloseClipboard() };
        }
    }

    /// A hidden message-only window that owns the clipboard while it is written (`SetClipboardData` fails after
    /// `EmptyClipboard` with no owner). Created and destroyed on the calling thread.
    struct Owner(HWND);

    impl Owner {
        fn create() -> Result<Self> {
            // SAFETY: the predefined STATIC class needs no registration; the window is message-only and is destroyed
            // by `Drop` on this thread.
            let hwnd = unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("STATIC"),
                    w!(""),
                    WINDOW_STYLE(0),
                    0,
                    0,
                    0,
                    0,
                    Some(HWND_MESSAGE),
                    None,
                    None,
                    None,
                )
            }
            .map_err(|e| os_err("CreateWindowExW", &e))?;
            Ok(Self(hwnd))
        }
    }

    impl Drop for Owner {
        fn drop(&mut self) {
            // SAFETY: the window was created by this thread.
            let _ = unsafe { DestroyWindow(self.0) };
        }
    }

    /// Copies one format's bytes out while the clipboard is open. `None` when the data is missing or over the cap.
    fn read_format(format: u32) -> Option<Vec<u8>> {
        // SAFETY: the clipboard is open (callers hold `Open`); the handle belongs to the clipboard and is only
        // locked, copied and unlocked here.
        unsafe {
            let handle = GetClipboardData(format).ok()?;
            let global = HGLOBAL(handle.0);
            let size = GlobalSize(global);
            if size == 0 || size > COPY_CAP {
                return None;
            }
            let ptr = GlobalLock(global);
            if ptr.is_null() {
                return None;
            }
            let copy = std::slice::from_raw_parts(ptr.cast::<u8>(), size).to_vec();
            let _ = GlobalUnlock(global);
            Some(copy)
        }
    }

    fn available(format: u32) -> bool {
        // SAFETY: plain query.
        format != 0 && unsafe { IsClipboardFormatAvailable(format) }.is_ok()
    }

    /// Puts `bytes` on the clipboard as `format` (the clipboard is open and emptied by the caller).
    fn set_format(format: u32, bytes: &[u8]) -> Result<()> {
        // SAFETY: the block is movable global memory filled before it is handed over; on success the system owns it,
        // on failure it is freed here.
        unsafe {
            let global = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).map_err(|e| os_err("GlobalAlloc", &e))?;
            let ptr = GlobalLock(global);
            if ptr.is_null() {
                let _ = GlobalFree(Some(global));
                return Err(PlatformError {
                    kind: ErrorKind::Os,
                    op: "GlobalLock",
                    os_code: None,
                    detail: String::new(),
                });
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.cast::<u8>(), bytes.len());
            let _ = GlobalUnlock(global);
            if let Err(e) = SetClipboardData(format, Some(HANDLE(global.0))) {
                let _ = GlobalFree(Some(global));
                return Err(os_err("SetClipboardData", &e));
            }
        }
        Ok(())
    }

    /// The clipboard of the logged-in user, for the user helper (and the macOS-free viewer side).
    pub struct WinClipboard {
        png: u32,
        exclude_monitor: u32,
        history: u32,
        viewer_ignore: u32,
    }

    impl Default for WinClipboard {
        fn default() -> Self {
            Self::new()
        }
    }

    impl WinClipboard {
        pub fn new() -> Self {
            Self {
                png: register(w!("PNG")),
                exclude_monitor: register(w!("ExcludeClipboardContentFromMonitorProcessing")),
                history: register(w!("CanIncludeInClipboardHistory")),
                viewer_ignore: register(w!("Clipboard Viewer Ignore")),
            }
        }

        fn concealed(&self) -> bool {
            let history = if available(self.history) {
                read_format(self.history).and_then(|b| b.get(..4).map(|d| u32::from_le_bytes(d.try_into().unwrap())))
            } else {
                None
            };
            is_concealed(available(self.exclude_monitor), history, available(self.viewer_ignore))
        }
    }

    impl ClipboardProvider for WinClipboard {
        fn change_count(&mut self) -> Result<u64> {
            // SAFETY: plain query, no clipboard access needed.
            Ok(u64::from(unsafe { GetClipboardSequenceNumber() }))
        }

        fn read(&mut self) -> Result<ClipRead> {
            let open = Open::with_retry(None)?;
            // SAFETY: plain query while the clipboard is open.
            if unsafe { CountClipboardFormats() } == 0 {
                return Ok(ClipRead::Empty);
            }
            if self.concealed() {
                return Ok(ClipRead::Concealed);
            }
            if available(u32::from(CF_UNICODETEXT.0)) {
                return Ok(read_format(u32::from(CF_UNICODETEXT.0))
                    .map_or(ClipRead::Unsupported, |b| ClipRead::Content(ClipContent::Text(decode_text(&b)))));
            }
            if available(self.png) {
                let png = read_format(self.png).and_then(|b| png_exact_len(&b).map(|n| b[..n].to_vec()));
                return Ok(png.map_or(ClipRead::Unsupported, |p| ClipRead::Content(ClipContent::Png(p))));
            }
            if available(u32::from(CF_DIB.0)) {
                let dib = read_format(u32::from(CF_DIB.0));
                // Compressing a 4K screen takes a moment: the clipboard is let go first, since every other program
                // that copies or pastes waits while it is open.
                drop(open);
                let png = dib.and_then(|b| dib_to_png(&b));
                return Ok(png.map_or(ClipRead::Unsupported, |p| ClipRead::Content(ClipContent::Png(p))));
            }
            Ok(ClipRead::Unsupported)
        }

        fn write(&mut self, content: &ClipContent) -> Result<()> {
            let (format, bytes) = match content {
                ClipContent::Text(t) if t.len() <= MAX_CLIPBOARD_TEXT as usize => {
                    (u32::from(CF_UNICODETEXT.0), encode_text(&windows_line_ends(t)))
                }
                ClipContent::Png(p) if p.len() <= MAX_CLIPBOARD_PNG as usize && png_exact_len(p) == Some(p.len()) => {
                    (self.png, p.clone())
                }
                _ => {
                    return Err(PlatformError {
                        kind: ErrorKind::InvalidInput,
                        op: "clipboard write",
                        os_code: None,
                        detail: "over the limit or not a PNG".into(),
                    });
                }
            };
            let owner = Owner::create()?;
            let _open = Open::with_retry(Some(owner.0))?;
            // SAFETY: the clipboard is open with `owner` as its owner.
            unsafe { EmptyClipboard() }.map_err(|e| os_err("EmptyClipboard", &e))?;
            set_format(format, &bytes)
        }
    }
}

#[cfg(windows)]
pub use win::WinClipboard;

#[cfg(test)]
mod tests {
    use super::*;

    /// The RGB rows of a PNG this module wrote: the IDAT inflated (the Adler-32 checked) and each row unfiltered.
    fn pixels(png: &[u8]) -> (u32, u32, Vec<u8>) {
        let c = chunks(png);
        let ihdr = &c[0].1;
        let (w, h) =
            (u32::from_be_bytes(ihdr[..4].try_into().unwrap()), u32::from_be_bytes(ihdr[4..8].try_into().unwrap()));
        assert_eq!(&ihdr[8..], &[8, 2, 0, 0, 0], "8-bit RGB");
        let z: Vec<u8> = c.iter().filter(|(k, _)| k == b"IDAT").flat_map(|(_, d)| d.clone()).collect();
        let raw = crate::deflate::inflate::zlib(&z).expect("a valid zlib stream");
        let stride = w as usize * 3;
        assert_eq!(raw.len(), (stride + 1) * h as usize);
        let mut rgb: Vec<u8> = Vec::with_capacity(stride * h as usize);
        for (y, row) in raw.chunks_exact(stride + 1).enumerate() {
            let start = rgb.len();
            for i in 0..stride {
                let a = if i >= 3 { rgb[start + i - 3] } else { 0 };
                let b = if y > 0 { rgb[start + i - stride] } else { 0 };
                let c = if y > 0 && i >= 3 { rgb[start + i - stride - 3] } else { 0 };
                let predicted = match row[0] {
                    0 => 0,
                    1 => a,
                    2 => b,
                    3 => ((u16::from(a) + u16::from(b)) / 2) as u8,
                    4 => paeth(a, b, c),
                    f => panic!("filter {f}"),
                };
                rgb.push(row[1 + i].wrapping_add(predicted));
            }
        }
        (w, h, rgb)
    }

    /// Splits a PNG into (kind, data) and checks each CRC.
    fn chunks(png: &[u8]) -> Vec<([u8; 4], Vec<u8>)> {
        assert_eq!(&png[..8], &PNG_SIGNATURE);
        let (mut at, mut v) = (8, Vec::new());
        while at < png.len() {
            let len = u32::from_be_bytes(png[at..at + 4].try_into().unwrap()) as usize;
            let kind: [u8; 4] = png[at + 4..at + 8].try_into().unwrap();
            let data = png[at + 8..at + 8 + len].to_vec();
            let crc = u32::from_be_bytes(png[at + 8 + len..at + 12 + len].try_into().unwrap());
            assert_eq!(crc, crc32(&[&kind, &data]), "crc of {kind:?}");
            v.push((kind, data));
            at += 12 + len;
        }
        v
    }

    fn dib(width: i32, height: i32, bits: u16, compression: u32, pixels: &[u8]) -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(&40u32.to_le_bytes());
        d.extend_from_slice(&width.to_le_bytes());
        d.extend_from_slice(&height.to_le_bytes());
        d.extend_from_slice(&1u16.to_le_bytes());
        d.extend_from_slice(&bits.to_le_bytes());
        d.extend_from_slice(&compression.to_le_bytes());
        d.extend_from_slice(&[0; 20]);
        if compression == 3 {
            for m in [0x00FF_0000u32, 0xFF00, 0xFF] {
                d.extend_from_slice(&m.to_le_bytes());
            }
        }
        d.extend_from_slice(pixels);
        d
    }

    #[test]
    fn crc_and_adler_known_values() {
        assert_eq!(crc32(&[b"123456789"]), 0xCBF4_3926);
        assert_eq!(crc32(&[b"1234", b"56789"]), 0xCBF4_3926, "over parts");
        assert_eq!(crate::deflate::adler32(b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    fn a_bottom_up_24_bit_dib_becomes_a_valid_top_down_png() {
        // 2x2, stride 8 (6 bytes + 2 padding), BGR, bottom row first.
        let bottom = [10, 20, 30, 40, 50, 60, 0, 0]; // pixels (30,20,10) (60,50,40) as RGB
        let top = [1, 2, 3, 4, 5, 6, 0, 0];
        let png = dib_to_png(&dib(2, 2, 24, 0, &[bottom, top].concat())).unwrap();
        let c = chunks(&png);
        assert_eq!(c.iter().map(|(k, _)| *k).collect::<Vec<_>>(), [*b"IHDR", *b"IDAT", *b"IEND"]);
        assert_eq!(c[0].1, [0, 0, 0, 2, 0, 0, 0, 2, 8, 2, 0, 0, 0]);
        assert_eq!(pixels(&png), (2, 2, vec![3, 2, 1, 6, 5, 4, 30, 20, 10, 60, 50, 40]));
        assert_eq!(png_exact_len(&png), Some(png.len()));
    }

    /// Review of 22/11/2026, D8: the compressed PNG keeps every pixel of a screen, whichever filter each row took.
    #[test]
    fn a_compressed_screen_png_keeps_every_pixel() {
        let (dib, rgb) = screen(640, 400);
        let png = dib_to_png(&dib).unwrap();
        assert_eq!(pixels(&png), (640, 400, rgb.clone()));
        assert!(png.len() * 10 < rgb.len(), "{} bytes for {} of pixels", png.len(), rgb.len());
        // Noise, which no filter helps: still exact, and no larger than the stored blocks would have been.
        let mut x: u32 = 7;
        let noise: Vec<u8> = (0..300 * 200 * 3)
            .map(|_| {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (x >> 24) as u8
            })
            .collect();
        let png = encode_png_rgb(300, 200, &noise);
        assert_eq!(pixels(&png), (300, 200, noise.clone()));
        assert!(png.len() < noise.len() + 200 + 1024, "{} bytes", png.len());
    }

    #[test]
    fn a_top_down_32_bit_bitfields_dib_is_read_and_unusual_ones_are_refused() {
        let px = [1, 2, 3, 255, 4, 5, 6, 255];
        let png = dib_to_png(&dib(1, -2, 32, 3, &px)).unwrap();
        assert_eq!(pixels(&png), (1, 2, vec![3, 2, 1, 6, 5, 4]));
        assert!(dib_to_png(&dib(1, 1, 16, 0, &[0; 4])).is_none(), "16 bit");
        assert!(dib_to_png(&dib(1, 1, 24, 1, &[0; 4])).is_none(), "RLE");
        assert!(dib_to_png(&dib(2, 2, 24, 0, &[0; 8])).is_none(), "short pixel data");
        assert!(dib_to_png(&[0; 10]).is_none());
    }

    /// A screen as a 32-bit bottom-up `CF_DIB`: a vertical gradient behind three white windows full of glyph-like ink
    /// (a different pseudo-random pattern per character cell) and a dark taskbar. Returns the DIB and its RGB rows,
    /// top first.
    fn screen(width: usize, height: usize) -> (Vec<u8>, Vec<u8>) {
        let mut rgb = vec![0u8; width * height * 3];
        let mut x_state: u32 = 0x9E37_79B9;
        let mut next = || {
            x_state ^= x_state << 13;
            x_state ^= x_state >> 17;
            x_state ^= x_state << 5;
            x_state
        };
        let windows =
            [(width / 20, height / 10, width / 2, height / 2), (width / 2, height / 3, width * 9 / 20, height / 2)];
        let cells: Vec<u32> = (0..(width / 8 + 1) * (height / 16 + 1)).map(|_| next()).collect();
        for y in 0..height {
            for x in 0..width {
                let px = &mut rgb[(y * width + x) * 3..][..3];
                let mut c =
                    [(30 + y * 120 / height) as u8, (60 + y * 80 / height) as u8, (120 + y * 100 / height) as u8];
                if y >= height - height / 36 {
                    c = [32, 32, 40];
                }
                for &(wx, wy, ww, wh) in &windows {
                    if (wx..wx + ww).contains(&x) && (wy..wy + wh).contains(&y) {
                        c = [255, 255, 255];
                        let (cx, cy) = (x / 8, y / 16);
                        let cell = cells[cy * (width / 8 + 1) + cx];
                        // Ink in rows 3 to 12 of a cell, a bit per pixel of the cell's own pattern, on most lines.
                        if (3..13).contains(&(y % 16)) && cy % 5 != 4 && (cell >> ((y % 16) + (x % 8))) & 1 == 1 {
                            c = [20, 20, 20];
                        }
                    }
                }
                px.copy_from_slice(&c);
            }
        }
        let mut pixels = Vec::with_capacity(width * height * 4);
        for y in (0..height).rev() {
            for &[r, g, b] in rgb[y * width * 3..(y + 1) * width * 3].as_chunks::<3>().0 {
                pixels.extend_from_slice(&[b, g, r, 0]);
            }
        }
        (dib(width as i32, height as i32, 32, 0, &pixels), rgb)
    }

    /// Review of 22/11/2026, D8: a screenshot (`CF_DIB`) was stored as a PNG with no compression, about 24 MiB at
    /// 3840x2160, past the 16 MiB limit of 7.5: copying the screen from 4K up never reached the other side. It is
    /// compressed now and fits with room to spare.
    #[test]
    fn a_4k_screenshot_becomes_a_png_within_the_clipboard_limit() {
        let (dib, _) = screen(3840, 2160);
        let png = dib_to_png(&dib).expect("a PNG");
        assert!(
            png.len() <= swapdesk_proto::MAX_CLIPBOARD_PNG as usize / 4,
            "{} bytes for a 4K screen, limit {}",
            png.len(),
            swapdesk_proto::MAX_CLIPBOARD_PNG
        );
        assert_eq!(png_exact_len(&png), Some(png.len()));
    }

    #[test]
    fn a_png_padded_by_global_memory_is_cut_at_iend() {
        let png = encode_png_rgb(1, 1, &[1, 2, 3]);
        let mut padded = png.clone();
        padded.extend_from_slice(&[0; 7]);
        assert_eq!(png_exact_len(&padded), Some(png.len()));
        assert_eq!(png_exact_len(&padded[..png.len() - 1]), None);
        assert_eq!(png_exact_len(b"not a png"), None);
    }

    #[test]
    fn text_round_trips_through_utf16_and_stops_at_the_nul() {
        let bytes = encode_text("olá, 世界 😀");
        assert_eq!(decode_text(&bytes), "olá, 世界 😀");
        let mut padded = bytes;
        padded.extend_from_slice(&[0x41, 0, 0x42, 0]);
        assert_eq!(decode_text(&padded), "olá, 世界 😀");
        assert_eq!(decode_text(&[0x00, 0xD8, 0x41, 0]), "\u{FFFD}A", "a lone surrogate is replaced");
        assert_eq!(decode_text(&[]), "");
    }

    /// Review of 22/11/2026, D9: text from a Mac (LF line ends) was written to the Windows clipboard as it came, where
    /// `CF_UNICODETEXT` ends every line with CR-LF, so programs that read only CR-LF (Notepad before 2018, many
    /// older ones) showed it on one line. A lone LF now becomes CR-LF; text that already has CR-LF (from Windows) and
    /// a lone CR are left alone.
    #[test]
    fn text_from_the_peer_gets_windows_line_ends_and_crlf_is_kept() {
        assert_eq!(windows_line_ends("a\nb\n"), "a\r\nb\r\n");
        assert_eq!(windows_line_ends("\n\n"), "\r\n\r\n");
        assert_eq!(windows_line_ends("a\r\nb"), "a\r\nb", "already Windows");
        assert_eq!(windows_line_ends("a\r\n\nb"), "a\r\n\r\nb", "mixed");
        assert_eq!(windows_line_ends("a\rb"), "a\rb", "a lone CR stays");
        assert_eq!(windows_line_ends("olá\n世界"), "olá\r\n世界");
        assert!(
            matches!(windows_line_ends("no line end"), std::borrow::Cow::Borrowed(_)),
            "no copy when nothing changes"
        );
        assert!(matches!(windows_line_ends("a\r\nb"), std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn the_concealed_rule() {
        assert!(!is_concealed(false, None, false));
        assert!(!is_concealed(false, Some(1), false));
        assert!(is_concealed(true, None, false));
        assert!(is_concealed(false, Some(0), false));
        assert!(is_concealed(false, None, true));
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "interactive: touches the clipboard"]
    fn interactive_text_and_png_round_trip_through_the_real_clipboard() {
        use swapdesk_platform::{ClipContent, ClipRead, ClipboardProvider};
        let mut cb = WinClipboard::new();
        // Save what the user had (text or an image PNG) and put it back at the end.
        let saved = cb.read().unwrap();
        if matches!(saved, ClipRead::Unsupported | ClipRead::Concealed) {
            eprintln!("the clipboard holds something this test would lose: skipped");
            return;
        }
        let before = cb.change_count().unwrap();
        let text = ClipContent::Text("swapdesk clipboard test \u{e1} \u{4e16}".into());
        cb.write(&text).unwrap();
        assert!(cb.change_count().unwrap() > before, "the sequence number moved");
        assert_eq!(cb.read().unwrap(), ClipRead::Content(text));
        let png = ClipContent::Png(encode_png_rgb(1, 1, &[9, 8, 7]));
        cb.write(&png).unwrap();
        assert_eq!(cb.read().unwrap(), ClipRead::Content(png));
        assert!(cb.write(&ClipContent::Png(vec![1, 2, 3])).is_err(), "not a PNG");
        match saved {
            ClipRead::Content(c) => cb.write(&c).unwrap(),
            _ => {
                // Empty: leave an empty text rather than the test's PNG.
                cb.write(&ClipContent::Text(String::new())).unwrap();
            }
        }
    }
}
