//! Pure conversion of the duplication's pointer shape buffers to the C3 `CursorShape` RGBA (straight alpha).

/// `DXGI_OUTDUPL_POINTER_SHAPE_TYPE_*` values.
pub const SHAPE_MONOCHROME: u32 = 1;
pub const SHAPE_COLOR: u32 = 2;
pub const SHAPE_MASKED_COLOR: u32 = 4;

/// A converted pointer image.
#[derive(Clone, PartialEq, Eq)]
pub struct Converted {
    pub width: u16,
    pub height: u16,
    pub rgba: Vec<u8>,
}

/// The pixels are whatever is under the pointer: `Debug` shows their size only (CONTRACTS.md C2.3).
impl core::fmt::Debug for Converted {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Converted")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("rgba", &format_args!("<redacted {} bytes>", self.rgba.len()))
            .finish()
    }
}

/// Converts one shape buffer. `width`, `height` and `pitch` are those of `DXGI_OUTDUPL_POINTER_SHAPE_INFO` (for a
/// monochrome shape the height covers both masks). `None` when the type is unknown, the size is zero or the buffer is
/// shorter than the sizes promise.
///
/// - Colour: BGRA to RGBA.
/// - Masked colour: alpha 0 in the source means "replace with this colour" (opaque); alpha 0xFF means "XOR with the
///   screen", approximated by the colour itself, and transparent when the colour is black (a no-op XOR).
/// - Monochrome (AND mask on top, XOR mask below): AND 0 / XOR 0 black, AND 0 / XOR 1 white, AND 1 / XOR 0
///   transparent, AND 1 / XOR 1 (invert screen) drawn black, so a text cursor stays visible on a light background.
///   The pointer is never composited into frames, so the viewer draws what is returned here.
pub fn convert_shape(shape_type: u32, width: u32, height: u32, pitch: u32, buf: &[u8]) -> Option<Converted> {
    if width == 0 || height == 0 || width > 512 || height > 1024 {
        return None;
    }
    match shape_type {
        SHAPE_COLOR | SHAPE_MASKED_COLOR => {
            let row_bytes = usize::try_from(width).ok()?.checked_mul(4)?;
            let pitch = usize::try_from(pitch).ok()?;
            if pitch < row_bytes || buf.len() < pitch.checked_mul(usize::try_from(height).ok()?)? - (pitch - row_bytes)
            {
                return None;
            }
            let mut rgba = Vec::with_capacity(row_bytes * height as usize);
            for y in 0..height as usize {
                let row = &buf[y * pitch..y * pitch + row_bytes];
                for px in row.as_chunks::<4>().0 {
                    let (b, g, r, a) = (px[0], px[1], px[2], px[3]);
                    if shape_type == SHAPE_COLOR {
                        rgba.extend_from_slice(&[r, g, b, a]);
                    } else if a == 0 {
                        rgba.extend_from_slice(&[r, g, b, 0xFF]);
                    } else if r | g | b == 0 {
                        rgba.extend_from_slice(&[0, 0, 0, 0]);
                    } else {
                        rgba.extend_from_slice(&[r, g, b, 0xFF]);
                    }
                }
            }
            Some(Converted { width: u16::try_from(width).ok()?, height: u16::try_from(height).ok()?, rgba })
        }
        SHAPE_MONOCHROME => {
            if !height.is_multiple_of(2) {
                return None;
            }
            let h = height / 2;
            let row_bytes = usize::try_from(width).ok()?.div_ceil(8);
            let pitch = usize::try_from(pitch).ok()?;
            if pitch < row_bytes || buf.len() < pitch.checked_mul(usize::try_from(height).ok()?)? - (pitch - row_bytes)
            {
                return None;
            }
            let bit = |row: usize, x: usize| (buf[row * pitch + x / 8] >> (7 - (x % 8))) & 1;
            let mut rgba = Vec::with_capacity(width as usize * h as usize * 4);
            for y in 0..h as usize {
                for x in 0..width as usize {
                    let and = bit(y, x);
                    let xor = bit(y + h as usize, x);
                    rgba.extend_from_slice(&match (and, xor) {
                        (0, 0) => [0, 0, 0, 0xFF],
                        (0, _) => [0xFF, 0xFF, 0xFF, 0xFF],
                        (_, 0) => [0, 0, 0, 0],
                        _ => [0, 0, 0, 0xFF],
                    });
                }
            }
            Some(Converted { width: u16::try_from(width).ok()?, height: u16::try_from(h).ok()?, rgba })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_of_a_converted_pointer_shows_the_size_and_never_the_pixels() {
        let shown = format!("{:?}", Converted { width: 2, height: 2, rgba: vec![233; 16] });
        assert!(!shown.split(|c: char| !c.is_ascii_digit()).any(|t| t == "233"), "pixels leaked: {shown}");
        assert!(shown.contains("<redacted 16 bytes>"), "{shown}");
    }

    #[test]
    fn color_shape_swaps_to_rgba_and_honours_the_pitch() {
        // 2x2, pitch 12 (4 bytes of padding per row).
        let mut buf = vec![0u8; 24];
        buf[0..4].copy_from_slice(&[1, 2, 3, 4]); // B G R A
        buf[4..8].copy_from_slice(&[10, 20, 30, 40]);
        buf[12..16].copy_from_slice(&[5, 6, 7, 8]);
        buf[16..20].copy_from_slice(&[50, 60, 70, 80]);
        let c = convert_shape(SHAPE_COLOR, 2, 2, 12, &buf).expect("valid");
        assert_eq!((c.width, c.height), (2, 2));
        assert_eq!(c.rgba, vec![3, 2, 1, 4, 30, 20, 10, 40, 7, 6, 5, 8, 70, 60, 50, 80]);
    }

    #[test]
    fn masked_color_distinguishes_replace_and_xor() {
        let buf = [
            1, 2, 3, 0, // replace: opaque colour
            9, 8, 7, 0xFF, // xor with a colour: drawn as that colour
            0, 0, 0, 0xFF, // xor with black: nothing
            0, 0, 0, 0, // replace with black: opaque black
        ];
        let c = convert_shape(SHAPE_MASKED_COLOR, 4, 1, 16, &buf).expect("valid");
        assert_eq!(c.rgba, vec![3, 2, 1, 255, 7, 8, 9, 255, 0, 0, 0, 0, 0, 0, 0, 255]);
    }

    #[test]
    fn monochrome_decodes_both_masks_msb_first() {
        // 8 wide, 2 high (one row of AND, one row of XOR); pitch 1.
        // x:     0 1 2 3 4 5 6 7
        // AND:   0 0 1 1 0 0 1 1  = 0b0011_0011
        // XOR:   0 1 0 1 0 1 0 1  = 0b0101_0101
        let c = convert_shape(SHAPE_MONOCHROME, 8, 2, 1, &[0b0011_0011, 0b0101_0101]).expect("valid");
        assert_eq!((c.width, c.height), (8, 1));
        let px: Vec<[u8; 4]> = c.rgba.as_chunks::<4>().0.to_vec();
        let black = [0, 0, 0, 255];
        let white = [255, 255, 255, 255];
        let clear = [0, 0, 0, 0];
        assert_eq!(px, vec![black, white, clear, black, black, white, clear, black]);
    }

    #[test]
    fn monochrome_width_not_a_multiple_of_eight_reads_the_leading_bits() {
        // 3 wide, 4 high -> 3x2 image, pitch 2 with padding.
        let buf = [0b1000_0000, 0, 0b0000_0000, 0, 0b0000_0000, 0, 0b1110_0000, 0];
        // Row 0: AND 100, XOR (row 2) 000 -> clear, black, black. Row 1: AND (row 1) 000, XOR (row 3) 111 -> white x3.
        let c = convert_shape(SHAPE_MONOCHROME, 3, 4, 2, &buf).expect("valid");
        assert_eq!((c.width, c.height), (3, 2));
        assert_eq!(&c.rgba[0..12], &[0, 0, 0, 0, 0, 0, 0, 255, 0, 0, 0, 255]);
        assert_eq!(&c.rgba[12..24], &[255; 12]);
    }

    #[test]
    fn refuses_bad_sizes_types_and_short_buffers() {
        assert!(convert_shape(SHAPE_COLOR, 0, 4, 16, &[0; 64]).is_none());
        assert!(convert_shape(SHAPE_COLOR, 4, 4, 8, &[0; 64]).is_none(), "pitch under the row");
        assert!(convert_shape(SHAPE_COLOR, 4, 4, 16, &[0; 63]).is_none(), "buffer short");
        assert!(convert_shape(SHAPE_MONOCHROME, 8, 3, 1, &[0; 3]).is_none(), "odd monochrome height");
        assert!(convert_shape(3, 4, 4, 16, &[0; 64]).is_none(), "unknown type");
        assert!(convert_shape(SHAPE_COLOR, 100_000, 4, 400_000, &[]).is_none(), "absurd width");
    }
}
