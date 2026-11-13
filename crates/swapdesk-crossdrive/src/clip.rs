//! The copy and paste proof across machines (v1.1 stage 12): text and an image, each way, through a live session.
//!
//! The viewer side writes to and reads from its own clipboard (the provider it is given); the orchestrator does the
//! same on the host machine with that OS's tools and they meet through files in the run folder:
//!
//! 1. this side copies a text, writes `clip-viewer-text` with it and waits for `clip-host-got-text`, which the
//!    orchestrator writes with what the host's clipboard then holds;
//! 2. the orchestrator copies a text on the host and writes it into `clip-host-text`; this side waits for its own
//!    clipboard to hold it;
//! 3. and 4. the same with a PNG image, compared by its width and height (`WxH`), since each OS may encode it again.
//!
//! Markers: `CLIP_TO_HOST_TEXT_OK`, `CLIP_FROM_HOST_TEXT_OK`, `CLIP_TO_HOST_IMAGE_OK`, `CLIP_FROM_HOST_IMAGE_OK`.

use std::path::Path;
use std::time::{Duration, Instant};

use swapdesk_platform::{ClipContent, ClipRead, ClipboardProvider};

use crate::pump::wait_file;

/// The image this side copies: an odd size, so a reply with the right size is not an accident.
pub const VIEWER_IMAGE: (u32, u32) = (37, 23);

/// Runs the four steps; `Err` names the first that failed.
pub fn clip_round(
    provider: &mut dyn ClipboardProvider,
    dir: &Path,
    timeout: Duration,
    log: &dyn Fn(&str),
) -> Result<(), String> {
    let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());

    // 1. Text from this machine to the host.
    let text = format!("swapdesk-clip-viewer-{nonce}");
    provider.write(&ClipContent::Text(text.clone())).map_err(|e| format!("copy a text here: {e:?}"))?;
    std::fs::write(dir.join("clip-viewer-text"), &text).map_err(|e| e.to_string())?;
    let got = read_reply(&dir.join("clip-host-got-text"), timeout)?;
    if got.trim() != text {
        return Err(format!("the host's clipboard holds {:?}, not {text:?}", got.trim()));
    }
    log("CLIP_TO_HOST_TEXT_OK");

    // 2. Text from the host to this machine.
    let want = read_reply(&dir.join("clip-host-text"), timeout)?.trim().to_owned();
    wait_clip(provider, timeout, |c| matches!(c, ClipRead::Content(ClipContent::Text(t)) if t.trim() == want))
        .map_err(|last| format!("this clipboard never held the host's text {want:?} (last: {last})"))?;
    log("CLIP_FROM_HOST_TEXT_OK");

    // 3. An image from this machine to the host.
    let (w, h) = VIEWER_IMAGE;
    provider.write(&ClipContent::Png(png(w, h))).map_err(|e| format!("copy an image here: {e:?}"))?;
    std::fs::write(dir.join("clip-viewer-png"), format!("{w}x{h}")).map_err(|e| e.to_string())?;
    let got = read_reply(&dir.join("clip-host-got-png"), timeout)?;
    if got.trim() != format!("{w}x{h}") {
        return Err(format!("the host's clipboard holds an image of {:?}, not {w}x{h}", got.trim()));
    }
    log("CLIP_TO_HOST_IMAGE_OK");

    // 4. An image from the host to this machine.
    let want = read_reply(&dir.join("clip-host-png"), timeout)?.trim().to_owned();
    wait_clip(provider, timeout, |c| match c {
        ClipRead::Content(ClipContent::Png(p)) => png_size(p).is_some_and(|(w, h)| format!("{w}x{h}") == want),
        _ => false,
    })
    .map_err(|last| format!("this clipboard never held the host's {want} image (last: {last})"))?;
    log("CLIP_FROM_HOST_IMAGE_OK");
    Ok(())
}

fn read_reply(path: &Path, timeout: Duration) -> Result<String, String> {
    wait_file(path, "the orchestrator's clipboard step", timeout)?;
    // The orchestrator writes the file whole; a moment for a slow disk.
    std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Polls the clipboard until `ok`; `Err` describes the last thing read.
fn wait_clip(
    provider: &mut dyn ClipboardProvider,
    timeout: Duration,
    ok: impl Fn(&ClipRead) -> bool,
) -> Result<(), String> {
    let end = Instant::now() + timeout;
    let mut last: String;
    loop {
        match provider.read() {
            Ok(c) if ok(&c) => return Ok(()),
            Ok(ClipRead::Content(ClipContent::Text(t))) => last = format!("text {t:?}"),
            Ok(ClipRead::Content(ClipContent::Png(p))) => last = format!("an image of {:?}", png_size(&p)),
            Ok(other) => last = format!("{other:?}"),
            Err(e) => last = format!("error {e:?}"),
        }
        if Instant::now() >= end {
            return Err(last);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The width and height in a PNG's header.
pub fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    if png.len() < 24 || &png[..8] != b"\x89PNG\r\n\x1a\n" || &png[12..16] != b"IHDR" {
        return None;
    }
    let be = |b: &[u8]| u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    Some((be(&png[16..20]), be(&png[20..24])))
}

/// A `w` by `h` RGB PNG with a diagonal gradient, its pixels in stored (uncompressed) deflate blocks.
pub fn png(w: u32, h: u32) -> Vec<u8> {
    let mut raw = Vec::with_capacity(((w * 3 + 1) * h) as usize);
    for y in 0..h {
        raw.push(0); // filter: none
        for x in 0..w {
            raw.extend_from_slice(&[(x * 255 / w.max(1)) as u8, (y * 255 / h.max(1)) as u8, 0x80]);
        }
    }
    let mut z = vec![0x78, 0x01];
    let mut chunks = raw.chunks(65_535).peekable();
    while let Some(c) = chunks.next() {
        z.push(u8::from(chunks.peek().is_none()));
        let len = c.len() as u16;
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(c);
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit RGB, deflate, no filter set, no interlace
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    for (kind, data) in [(b"IHDR", ihdr.as_slice()), (b"IDAT", z.as_slice()), (b"IEND", &[][..])] {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let crc = crc32(&out[start..]);
        out.extend_from_slice(&crc.to_be_bytes());
    }
    out
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + u32::from(x)) % 65_521;
        b = (b + a) % 65_521;
    }
    b << 16 | a
}

fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &x in data {
        c ^= u32::from(x);
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    !c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_png_says_its_size_and_carries_the_known_checksums() {
        let p = png(37, 23);
        assert_eq!(png_size(&p), Some((37, 23)));
        assert_eq!(&p[p.len() - 12..p.len() - 4], b"\0\0\0\0IEND", "IEND last, empty");
        // CRC-32 of "IEND" is the value every PNG ends with.
        assert_eq!(&p[p.len() - 4..], &0xAE42_6082u32.to_be_bytes());
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    fn a_truncated_or_foreign_header_has_no_size() {
        assert_eq!(png_size(b"\x89PNG"), None);
        assert_eq!(png_size(&[0u8; 32]), None);
    }
}
