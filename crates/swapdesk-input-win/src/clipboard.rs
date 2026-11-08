//! `ClipboardProvider` (C3) on the Windows clipboard (F-CLIP-01, PROTOCOL.md 7.5): UTF-8 text through
//! `CF_UNICODETEXT` and PNG through the registered `"PNG"` format, with `CF_DIB` (screenshots, "copy image") converted to
//! PNG on read. Content a password manager marked as hidden (`ExcludeClipboardContentFromMonitorProcessing`,
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
//! The pure parts (DIB to PNG, the PNG length, UTF-16 decoding, the concealed rule) build on every host.

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

fn crc32(data: &[&[u8]]) -> u32 {
    let mut crc = u32::MAX;
    for part in data {
        for &b in *part {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
            }
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + u32::from(x)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(&[kind, data]).to_be_bytes());
}

/// A PNG of 8-bit RGB rows in zlib "stored" blocks (no compression: a clipboard image is local and the peer's link
/// is a LAN; the 16 MiB limit of 7.5 is checked by the sync engine).
pub(crate) fn encode_png_rgb(width: u32, height: u32, rgb_rows: &[u8]) -> Vec<u8> {
    let stride = width as usize * 3;
    let mut raw = Vec::with_capacity((stride + 1) * height as usize);
    for row in rgb_rows.chunks_exact(stride) {
        raw.push(0); // filter: none
        raw.extend_from_slice(row);
    }
    let mut z = vec![0x78, 0x01];
    let mut blocks = raw.chunks(65535).peekable();
    if blocks.peek().is_none() {
        z.extend_from_slice(&[1, 0, 0, 0xFF, 0xFF]);
    }
    while let Some(b) = blocks.next() {
        z.push(u8::from(blocks.peek().is_none()));
        z.extend_from_slice(&(b.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(b.len() as u16)).to_le_bytes());
        z.extend_from_slice(b);
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());
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

    use super::{COPY_CAP, decode_text, dib_to_png, encode_text, is_concealed, png_exact_len};

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
            let _open = Open::with_retry(None)?;
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
                let png = read_format(u32::from(CF_DIB.0)).and_then(|b| dib_to_png(&b));
                return Ok(png.map_or(ClipRead::Unsupported, |p| ClipRead::Content(ClipContent::Png(p))));
            }
            Ok(ClipRead::Unsupported)
        }

        fn write(&mut self, content: &ClipContent) -> Result<()> {
            let (format, bytes) = match content {
                ClipContent::Text(t) if t.len() <= MAX_CLIPBOARD_TEXT as usize => {
                    (u32::from(CF_UNICODETEXT.0), encode_text(t))
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

    /// Inflates the stored blocks of a zlib stream (all this module writes) and checks the Adler-32.
    fn unstore(z: &[u8]) -> Vec<u8> {
        assert_eq!(&z[..2], &[0x78, 0x01]);
        let (mut at, mut out) = (2, Vec::new());
        loop {
            let last = z[at] & 1 == 1;
            assert_eq!(z[at] & 0b110, 0, "stored block");
            let len = u16::from_le_bytes([z[at + 1], z[at + 2]]);
            assert_eq!(u16::from_le_bytes([z[at + 3], z[at + 4]]), !len);
            out.extend_from_slice(&z[at + 5..at + 5 + len as usize]);
            at += 5 + len as usize;
            if last {
                break;
            }
        }
        assert_eq!(&z[at..], &adler32(&out).to_be_bytes());
        out
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
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
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
        let raw = unstore(&c[1].1);
        assert_eq!(raw, [0, 3, 2, 1, 6, 5, 4, 0, 30, 20, 10, 60, 50, 40]);
        assert_eq!(png_exact_len(&png), Some(png.len()));
    }

    #[test]
    fn a_top_down_32_bit_bitfields_dib_is_read_and_unusual_ones_are_refused() {
        let px = [1, 2, 3, 255, 4, 5, 6, 255];
        let png = dib_to_png(&dib(1, -2, 32, 3, &px)).unwrap();
        assert_eq!(unstore(&chunks(&png)[1].1), [0, 3, 2, 1, 0, 6, 5, 4]);
        assert!(dib_to_png(&dib(1, 1, 16, 0, &[0; 4])).is_none(), "16 bit");
        assert!(dib_to_png(&dib(1, 1, 24, 1, &[0; 4])).is_none(), "RLE");
        assert!(dib_to_png(&dib(2, 2, 24, 0, &[0; 8])).is_none(), "short pixel data");
        assert!(dib_to_png(&[0; 10]).is_none());
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
