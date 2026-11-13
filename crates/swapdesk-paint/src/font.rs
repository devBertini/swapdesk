//! The embedded typeface and the CSS line box both shells lay text out in, so a line of text sits at the same place
//! on Windows and on macOS.

use swapdesk_ui::tokens::TextStyle;

/// Instrument Sans (SIL OFL 1.1, assets/OFL.txt), embedded so nothing is installed system-wide.
pub static INSTRUMENT_SANS: &[u8] = include_bytes!("../assets/InstrumentSans[wdth,wght].ttf");

/// Ascent and descent of a TrueType face as fractions of its em, as DirectWrite reports them: the OS/2 typographic
/// values when the face sets USE_TYPO_METRICS (fsSelection bit 7), else the OS/2 Windows values. `None` for bytes that
/// are not a face with `head` and `OS/2` tables.
pub fn face_metrics(font: &[u8]) -> Option<(f32, f32)> {
    let u16_at = |at: usize| font.get(at..at + 2).map(|b| u16::from_be_bytes([b[0], b[1]]));
    let u32_at = |at: usize| font.get(at..at + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    let tables = usize::from(u16_at(4)?);
    let table = |tag: &[u8; 4]| {
        (0..tables).find_map(|i| {
            let rec = 12 + i * 16;
            (font.get(rec..rec + 4)? == tag).then(|| u32_at(rec + 8).map(|o| o as usize)).flatten()
        })
    };
    let head = table(b"head")?;
    let os2 = table(b"OS/2")?;
    let em = f32::from(u16_at(head + 18)?);
    if em == 0.0 {
        return None;
    }
    let fs_selection = u16_at(os2 + 62)?;
    let (ascent, descent) = if fs_selection & (1 << 7) != 0 {
        (f32::from(u16_at(os2 + 68)? as i16), -f32::from(u16_at(os2 + 70)? as i16))
    } else {
        (f32::from(u16_at(os2 + 74)?), f32::from(u16_at(os2 + 76)?))
    };
    Some((ascent / em, descent / em))
}

/// The line height of a style and the distance from the top of its line box to the baseline, in DIPs. CSS puts the
/// content area in the middle of the line box; browsers lay text out in device pixels and round the font's ascent and
/// descent there, so the half-leading is worked out the same way.
pub fn line_box(st: TextStyle, scale: f32, ascent: f32, descent: f32) -> (f32, f32) {
    let line_height = st.size * st.line;
    let ascent_px = (ascent * st.size * scale).round();
    let descent_px = (descent * st.size * scale).round();
    let half_leading = ((line_height * scale - (ascent_px + descent_px)) / 2.0).floor();
    (line_height, (half_leading + ascent_px) / scale)
}

/// The height a label reports for `raw` DIPs of laid-out lines: browsers keep layout sizes in 1/64 device pixels,
/// rounded down, and sums of them decide where rows land.
pub fn label_height(raw: f32, scale: f32) -> f32 {
    (raw * scale * 64.0).floor() / 64.0 / scale
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_face_has_metrics_and_garbage_has_none() {
        let (a, d) = face_metrics(INSTRUMENT_SANS).expect("metrics of the embedded face");
        assert!(a > 0.7 && a < 1.3 && d > 0.1 && d < 0.5, "ascent {a}, descent {d}");
        assert!(face_metrics(b"not a font").is_none());
        assert!(face_metrics(&[]).is_none());
    }

    #[test]
    fn the_baseline_sits_inside_the_line_box_at_every_scale() {
        let (a, d) = face_metrics(INSTRUMENT_SANS).unwrap();
        let st = TextStyle::new(14.0, 400);
        for scale in [1.0, 1.25, 1.5, 2.0] {
            let (lh, base) = line_box(st, scale, a, d);
            assert!(base > 0.0 && base < lh, "scale {scale}: baseline {base} of {lh}");
            assert_eq!((base * scale).fract(), 0.0, "scale {scale}: the baseline lands on a device pixel");
        }
        assert_eq!(label_height(20.3, 1.0), 20.296875);
    }
}
