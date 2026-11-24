//! Text, measured by Android and laid out here.
//!
//! The screens measure every label while they paint (`Painter::layout`), and only Android can measure text in the
//! phone's fonts. Nothing here calls into Java. Instead a frame is painted against a cache of measures: a label the
//! cache does not know gets a rough size and is remembered as a need, and a frame that met needs is thrown away and
//! answered with the list of them ([`TextCache::take_needs`], [`encode_needs`]). Java measures the list in one go
//! (`TextMeasurer.java`), hands the results back ([`TextCache::measured`]), and asks for the frame again. A screen
//! that shows text it has shown before costs one pass; a new screen costs two, sometimes three (a size that changes
//! moves other text).
//!
//! What Java returns for a label is its lines, each as runs of text with their widths in device pixels (a run is
//! semi-bold or plain; an ellipsized line comes back already trimmed). Where the lines go, their height and their
//! baseline are worked out here with the rules every shell shares (`swapdesk_paint::line_box`).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crate::canvas_ops::{NEEDS, Reader, Writer, flag};

/// Most labels the cache keeps before it starts over (a long session of changing text must not grow without end).
pub const CACHE_MAX: usize = 4096;
/// Most needs sent to Java in one list; a frame with more asks again after these are measured.
pub const NEEDS_MAX: usize = 512;

/// What makes two labels the same measure. Sizes are in device pixels, so a change of display scale measures anew.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TextKey {
    pub text: String,
    /// The bits of the font size in device pixels.
    pub size_px: u32,
    pub weight: u16,
    /// The bits of the tracking, in ems.
    pub tracking: u32,
    pub mono: bool,
    pub italic: bool,
    /// The width the text is laid out in, in whole device pixels.
    pub max_w_px: i32,
    /// One line, trimmed with an ellipsis at the width; else wrapped at it.
    pub ellipsis: bool,
    /// The first characters (Unicode scalar values) that are semi-bold; never more than the text has.
    pub bold_chars: u16,
}

/// A stretch of one line in one weight.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    pub bold: bool,
    /// In device pixels.
    pub width: f32,
    pub text: String,
}

/// A label as Android measured it: its lines, each a list of runs.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Measured {
    pub lines: Vec<Vec<Run>>,
}

impl Measured {
    pub fn line_width(line: &[Run]) -> f32 {
        line.iter().map(|r| r.width).sum()
    }

    /// The widest line, in device pixels.
    pub fn width(&self) -> f32 {
        self.lines.iter().map(|l| Self::line_width(l)).fold(0.0, f32::max)
    }
}

/// The measures known so far and the needs of the frame being painted. One per window, kept across frames.
#[derive(Default)]
pub struct TextCache {
    known: RefCell<HashMap<TextKey, Rc<Measured>>>,
    needs: RefCell<Vec<TextKey>>,
    needed: RefCell<HashSet<TextKey>>,
}

impl TextCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The measure of `key`, or `None` after remembering it as a need of this frame.
    pub fn get(&self, key: &TextKey) -> Option<Rc<Measured>> {
        if let Some(hit) = self.known.borrow().get(key) {
            return Some(Rc::clone(hit));
        }
        if self.needs.borrow().len() < NEEDS_MAX && self.needed.borrow_mut().insert(key.clone()) {
            self.needs.borrow_mut().push(key.clone());
        }
        None
    }

    /// Whether the frame painted since the last [`Self::take_needs`] met a label without a measure.
    pub fn has_needs(&self) -> bool {
        !self.needs.borrow().is_empty()
    }

    /// The needs of the frame just painted, in the order they were met; the list is empty afterwards.
    pub fn take_needs(&self) -> Vec<TextKey> {
        self.needed.borrow_mut().clear();
        std::mem::take(&mut self.needs.borrow_mut())
    }

    /// Takes Java's answer to `needs` (the list [`Self::take_needs`] gave, in its order). Returns how many labels
    /// were learned.
    pub fn measured(&self, needs: &[TextKey], answer: &[u8]) -> Result<usize, &'static str> {
        let results = decode_measured(answer)?;
        if results.len() != needs.len() {
            return Err("the measures do not match the needs");
        }
        let mut known = self.known.borrow_mut();
        if known.len() + needs.len() > CACHE_MAX {
            known.clear();
        }
        for (key, measured) in needs.iter().zip(results) {
            known.insert(key.clone(), Rc::new(measured));
        }
        Ok(needs.len())
    }

    /// How many labels are known.
    pub fn len(&self) -> usize {
        self.known.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The list of needs for Java, every number little-endian:
///
/// ```text
/// u8 NEEDS, u16 count, then per label:
///   f32 size (device pixels), u16 weight, f32 tracking (ems), u8 flags (mono, italic, ellipsis),
///   f32 width to lay out in (device pixels), u16 semi-bold start in UTF-16 units (an index into Java's string),
///   u16 length, the text (UTF-8)
/// ```
pub fn encode_needs(needs: &[TextKey]) -> Vec<u8> {
    let mut w = Writer::default();
    w.u8(NEEDS);
    w.u16(needs.len() as u16);
    for k in needs {
        w.f32(f32::from_bits(k.size_px));
        w.u16(k.weight);
        w.f32(f32::from_bits(k.tracking));
        w.u8(flag::of(k.mono, k.italic, k.ellipsis));
        w.f32(k.max_w_px as f32);
        let bold_utf16: usize = k.text.chars().take(usize::from(k.bold_chars)).map(char::len_utf16).sum();
        w.u16(bold_utf16.min(usize::from(u16::MAX)) as u16);
        w.str(&k.text);
    }
    w.0
}

/// Java's answer, every number little-endian:
///
/// ```text
/// u16 count, then per label, in the order of the needs:
///   u16 lines, then per line: u8 runs, then per run: u8 bold (0 or 1), f32 width (device pixels), u16 length, text
/// ```
pub fn decode_measured(bytes: &[u8]) -> Result<Vec<Measured>, &'static str> {
    let mut r = Reader(bytes);
    let count = usize::from(r.u16()?);
    let mut out = Vec::with_capacity(count.min(NEEDS_MAX));
    for _ in 0..count {
        let lines = usize::from(r.u16()?);
        let mut measured = Measured { lines: Vec::with_capacity(lines.min(r.0.len())) };
        for _ in 0..lines {
            let runs = usize::from(r.u8()?);
            let mut line = Vec::with_capacity(runs);
            for _ in 0..runs {
                let bold = r.u8()? != 0;
                let width = r.f32()?;
                if !width.is_finite() || width < 0.0 {
                    return Err("a run's width is not a size");
                }
                line.push(Run { bold, width, text: r.str()? });
            }
            measured.lines.push(line);
        }
        out.push(measured);
    }
    if !r.0.is_empty() {
        return Err("bytes are left over after the measures");
    }
    Ok(out)
}

/// The answer as Java writes it, for the tests and for a measurer written in Rust.
pub fn encode_measured(results: &[Measured]) -> Vec<u8> {
    let mut w = Writer::default();
    w.u16(results.len() as u16);
    for m in results {
        w.u16(m.lines.len() as u16);
        for line in &m.lines {
            w.u8(line.len() as u8);
            for run in line {
                w.u8(u8::from(run.bold));
                w.f32(run.width);
                w.str(&run.text);
            }
        }
    }
    w.0
}

/// The needs of a list as Java reads them, for the tests and for a measurer written in Rust.
pub fn decode_needs(bytes: &[u8]) -> Result<Vec<TextKey>, &'static str> {
    let mut r = Reader(bytes);
    if r.u8()? != NEEDS {
        return Err("not a list of needs");
    }
    let count = usize::from(r.u16()?);
    let mut out = Vec::with_capacity(count.min(NEEDS_MAX));
    for _ in 0..count {
        let size_px = r.f32()?.to_bits();
        let weight = r.u16()?;
        let tracking = r.f32()?.to_bits();
        let flags = r.u8()?;
        let max_w_px = r.f32()? as i32;
        let bold_utf16 = usize::from(r.u16()?);
        let text = r.str()?;
        let mut units = 0;
        let bold_chars = text.chars().take_while(|c| {
            units += c.len_utf16();
            units <= bold_utf16
        });
        let bold_chars = bold_chars.count() as u16;
        out.push(TextKey {
            text,
            size_px,
            weight,
            tracking,
            mono: flags & flag::MONO != 0,
            italic: flags & flag::ITALIC != 0,
            max_w_px,
            ellipsis: flags & flag::ELLIPSIS != 0,
            bold_chars,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(text: &str, max_w_px: i32) -> TextKey {
        TextKey {
            text: text.into(),
            size_px: 36.75f32.to_bits(),
            weight: 400,
            tracking: 0f32.to_bits(),
            mono: false,
            italic: false,
            max_w_px,
            ellipsis: false,
            bold_chars: 0,
        }
    }

    fn one_line(text: &str, width: f32) -> Measured {
        Measured { lines: vec![vec![Run { bold: false, width, text: text.into() }]] }
    }

    #[test]
    fn a_label_is_needed_once_then_known() {
        let cache = TextCache::new();
        let (a, b) = (key("Início", 500), key("Pareados", 500));
        assert!(cache.get(&a).is_none() && cache.get(&b).is_none() && cache.get(&a).is_none());
        assert!(cache.has_needs());
        let needs = cache.take_needs();
        assert_eq!(needs, vec![a.clone(), b.clone()], "each once, in the order met");
        assert!(!cache.has_needs());

        let answer = encode_measured(&[one_line("Início", 71.5), one_line("Pareados", 120.0)]);
        assert_eq!(cache.measured(&needs, &answer), Ok(2));
        assert_eq!(cache.get(&a).unwrap().width(), 71.5);
        assert_eq!(cache.get(&b).unwrap().lines[0][0].text, "Pareados");
        assert!(!cache.has_needs(), "known labels are no need");
        // The same text at another width is another measure.
        assert!(cache.get(&key("Início", 40)).is_none());
    }

    #[test]
    fn the_two_lists_read_back_and_a_bad_answer_is_refused() {
        let needs = vec![
            key("Você controla", 300),
            TextKey { mono: true, italic: true, ellipsis: true, bold_chars: 4, weight: 600, ..key("ls -la", 99) },
            // Outside the basic plane a character is two of Java's units: the count crosses as an index there.
            TextKey { bold_chars: 3, ..key("a😀b c", 300) },
        ];
        assert_eq!(decode_needs(&encode_needs(&needs)).unwrap(), needs);

        let rich = Measured {
            lines: vec![
                vec![
                    Run { bold: true, width: 40.0, text: "Sala".into() },
                    Run { bold: false, width: 80.5, text: " controla".into() },
                ],
                vec![Run { bold: false, width: 60.0, text: "este PC".into() }],
            ],
        };
        let answer = encode_measured(&[one_line("a", 1.0), rich.clone()]);
        assert_eq!(decode_measured(&answer).unwrap(), vec![one_line("a", 1.0), rich.clone()]);
        // "a😀b" is three characters and four of Java's units.
        let wire = encode_needs(&needs[2..]);
        assert_eq!(u16::from_le_bytes([wire[18], wire[19]]), 4, "the semi-bold start as Java indexes it");
        assert_eq!((rich.width(), Measured::default().width()), (120.5, 0.0));

        let cache = TextCache::new();
        assert_eq!(cache.measured(&needs[..1], &answer), Err("the measures do not match the needs"));
        for cut in 0..answer.len() {
            assert!(decode_measured(&answer[..cut]).is_err(), "cut at {cut}");
        }
        let mut longer = answer.clone();
        longer.push(0);
        assert!(decode_measured(&longer).is_err());
        let mut negative = encode_measured(&[one_line("a", 1.0)]);
        // The count, the lines, the runs and the bold flag come before the first width.
        negative[6..10].copy_from_slice(&(-1.0f32).to_le_bytes());
        assert_eq!(decode_measured(&negative), Err("a run's width is not a size"));
    }

    #[test]
    fn the_cache_starts_over_rather_than_grow_without_end() {
        let cache = TextCache::new();
        for round in 0..3 {
            let needs: Vec<TextKey> = (0..NEEDS_MAX).map(|i| key(&format!("{round}-{i}"), 100)).collect();
            let answer = encode_measured(&vec![one_line("x", 1.0); needs.len()]);
            for chunk in 0..(CACHE_MAX / NEEDS_MAX) {
                let batch: Vec<TextKey> =
                    needs.iter().map(|k| TextKey { text: format!("{}-{chunk}", k.text), ..k.clone() }).collect();
                cache.measured(&batch, &answer).unwrap();
            }
            assert!(cache.len() <= CACHE_MAX, "round {round}: {}", cache.len());
        }
        assert!(!cache.is_empty());
    }
}
