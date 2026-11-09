//! What the Mac clipboard holds, as pure decisions (no OS call, unit tested on every host): which of the pasteboard's
//! type identifiers wins, the concealed rule, and the exact length of a PNG. [`crate::MacClipboard`] reads the types
//! and bytes from `NSPasteboard` and asks this module what they mean.
//!
//! Type identifiers: `public.utf8-plain-text` is `NSPasteboardTypeString`, `public.png` is `NSPasteboardTypePNG`,
//! `public.tiff` is `NSPasteboardTypeTIFF` (what screenshots and "copy image" put there). Password managers mark
//! secrets with `org.nspasteboard.ConcealedType` (nspasteboard.org); such content is never read.

pub const TYPE_STRING: &str = "public.utf8-plain-text";
pub const TYPE_PNG: &str = "public.png";
pub const TYPE_TIFF: &str = "public.tiff";
pub const TYPE_CONCEALED: &str = "org.nspasteboard.ConcealedType";

/// What to read from the pasteboard, in the order of preference of the Windows provider: text, PNG, then TIFF
/// converted to PNG.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    Empty,
    Concealed,
    Text,
    Png,
    TiffAsPng,
    Unsupported,
}

pub fn pick(types: &[String]) -> Pick {
    if types.is_empty() {
        return Pick::Empty;
    }
    let has = |t: &str| types.iter().any(|x| x == t);
    if has(TYPE_CONCEALED) {
        Pick::Concealed
    } else if has(TYPE_STRING) {
        Pick::Text
    } else if has(TYPE_PNG) {
        Pick::Png
    } else if has(TYPE_TIFF) {
        Pick::TiffAsPng
    } else {
        Pick::Unsupported
    }
}

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// The real length of a PNG in a buffer that may be longer: up to the end of its `IEND` chunk. `None` when it is not a
/// well-formed chunk sequence.
pub fn png_exact_len(data: &[u8]) -> Option<usize> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn types(t: &[&str]) -> Vec<String> {
        t.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn text_wins_over_images_and_concealed_wins_over_everything() {
        assert_eq!(pick(&[]), Pick::Empty);
        assert_eq!(pick(&types(&[TYPE_PNG, TYPE_STRING])), Pick::Text);
        assert_eq!(pick(&types(&[TYPE_TIFF, TYPE_PNG])), Pick::Png);
        assert_eq!(pick(&types(&[TYPE_TIFF])), Pick::TiffAsPng);
        assert_eq!(pick(&types(&["public.html"])), Pick::Unsupported);
        assert_eq!(pick(&types(&[TYPE_STRING, TYPE_CONCEALED])), Pick::Concealed);
    }

    fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut c = (data.len() as u32).to_be_bytes().to_vec();
        c.extend_from_slice(kind);
        c.extend_from_slice(data);
        c.extend_from_slice(&[0, 0, 0, 0]); // the CRC is not checked here
        c
    }

    #[test]
    fn png_length_ends_at_iend_and_rejects_the_malformed() {
        let mut png = PNG_SIGNATURE.to_vec();
        png.extend(chunk(b"IHDR", &[0; 13]));
        png.extend(chunk(b"IEND", &[]));
        let exact = png.len();
        assert_eq!(png_exact_len(&png), Some(exact));
        png.extend_from_slice(&[0; 5]);
        assert_eq!(png_exact_len(&png), Some(exact), "trailing padding is ignored");
        assert_eq!(png_exact_len(&png[..exact - 1]), None, "cut inside IEND");
        assert_eq!(png_exact_len(b"not a png"), None);
        assert_eq!(png_exact_len(&[]), None);
    }
}
