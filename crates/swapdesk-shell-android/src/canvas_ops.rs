//! One frame as bytes: what the recorder writes and the Java view replays on its canvas (`PaintView.java`).
//!
//! Every number is little-endian; lengths and coordinates are device pixels (`f32`), colours are `0xAARRGGBB`
//! (`u32`), a string is its UTF-8 length as `u16` and its bytes. The buffer starts with one byte, [`FRAME`] or
//! [`NEEDS`], and a frame is then a run of operations, each one byte ([`op`]) and its fields in the order of the
//! [`Op`] variant of the same name. [`decode`] is the format's reference: the tests read frames back with it, and the
//! Java replay follows it field for field.
//!
//! The replay holds no geometry of its own. Icons, arcs and shadows reach it already worked out by the routines
//! Windows and the Mac draw with (`swapdesk_paint::svg`), as paths and plain rounded rectangles.

use swapdesk_paint::svg::Seg;
use swapdesk_ui::tokens::Color;

/// First byte of a frame's buffer.
pub const FRAME: u8 = 0;
/// First byte of a buffer that asks for text measures instead (see `crate::text_measure`).
pub const NEEDS: u8 = 1;

/// The operation codes.
pub mod op {
    pub const CLEAR: u8 = 1;
    pub const FILL_RECT: u8 = 2;
    pub const FILL_RRECT: u8 = 3;
    pub const STROKE_RRECT: u8 = 4;
    pub const STROKE_RRECT_DASHED: u8 = 5;
    pub const LINE: u8 = 6;
    pub const FILL_CIRCLE: u8 = 7;
    pub const STROKE_CIRCLE: u8 = 8;
    pub const STROKE_PATH: u8 = 9;
    pub const TRIANGLE: u8 = 10;
    pub const SHADOW_LAYER: u8 = 11;
    pub const PUSH_CLIP: u8 = 12;
    pub const POP_CLIP: u8 = 13;
    pub const TEXT: u8 = 14;
}

/// The kinds of a path's steps.
pub mod seg {
    pub const MOVE: u8 = 0;
    pub const LINE: u8 = 1;
    pub const CUBIC: u8 = 2;
    pub const CLOSE: u8 = 3;
}

/// Text flags of [`Op::Text`] and of a measure request.
pub mod flag {
    pub const MONO: u8 = 1;
    pub const ITALIC: u8 = 2;
    /// A measure request only: one line, trimmed with an ellipsis at the width.
    pub const ELLIPSIS: u8 = 4;

    /// The bits of a style.
    pub fn of(mono: bool, italic: bool, ellipsis: bool) -> u8 {
        (if mono { MONO } else { 0 }) | (if italic { ITALIC } else { 0 }) | (if ellipsis { ELLIPSIS } else { 0 })
    }
}

pub fn argb(c: Color) -> u32 {
    u32::from(c.a) << 24 | u32::from(c.r) << 16 | u32::from(c.g) << 8 | u32::from(c.b)
}

/// One operation of a frame. Rectangles are `[x, y, w, h]`.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    Clear(u32),
    FillRect([f32; 4], u32),
    FillRrect([f32; 4], f32, u32),
    /// The rectangle the stroke is centred on, its corner radius, the stroke's width; square ends, mitred corners.
    StrokeRrect([f32; 4], f32, f32, u32),
    /// The same with dashes and gaps two stroke widths long, starting at the path's start.
    StrokeRrectDashed([f32; 4], f32, f32, u32),
    /// From, to, width, colour, round ends.
    Line([f32; 2], [f32; 2], f32, u32, bool),
    FillCircle([f32; 2], f32, u32),
    StrokeCircle([f32; 2], f32, f32, u32),
    /// A stroked path with round ends and joins: its width, colour and steps.
    StrokePath(f32, u32, Vec<Seg>),
    Triangle([f32; 6], u32),
    /// One layer of a soft shadow: a filled rounded rectangle whose colour (`0x00RRGGBB`) has an alpha finer than
    /// 1/255, given as a fraction.
    ShadowLayer([f32; 4], f32, u32, f32),
    PushClip([f32; 4]),
    PopClip,
    /// One run of text: where its baseline starts, its ink, size, weight, tracking in ems, [`flag`] bits.
    Text {
        at: [f32; 2],
        color: u32,
        size: f32,
        weight: u16,
        tracking: f32,
        flags: u8,
        text: String,
    },
}

/// Appends to a buffer.
#[derive(Default)]
pub struct Writer(pub Vec<u8>);

impl Writer {
    pub fn u8(&mut self, v: u8) {
        self.0.push(v);
    }

    pub fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    pub fn f32(&mut self, v: f32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    pub fn floats(&mut self, v: &[f32]) {
        for f in v {
            self.f32(*f);
        }
    }

    /// A string of at most 65535 bytes; a longer one is cut at a character boundary.
    pub fn str(&mut self, s: &str) {
        let mut end = s.len().min(usize::from(u16::MAX));
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        self.u16(end as u16);
        self.0.extend_from_slice(&s.as_bytes()[..end]);
    }

    /// A path of at most 65535 steps; a longer one is cut.
    pub fn segs(&mut self, segs: &[Seg]) {
        let segs = &segs[..segs.len().min(usize::from(u16::MAX))];
        self.u16(segs.len() as u16);
        for s in segs {
            match *s {
                Seg::Move(x, y) => {
                    self.u8(seg::MOVE);
                    self.floats(&[x, y]);
                }
                Seg::Line(x, y) => {
                    self.u8(seg::LINE);
                    self.floats(&[x, y]);
                }
                Seg::Cubic(x1, y1, x2, y2, x, y) => {
                    self.u8(seg::CUBIC);
                    self.floats(&[x1, y1, x2, y2, x, y]);
                }
                Seg::Close => self.u8(seg::CLOSE),
            }
        }
    }

    pub fn push(&mut self, o: &Op) {
        match o {
            Op::Clear(c) => {
                self.u8(op::CLEAR);
                self.u32(*c);
            }
            Op::FillRect(r, c) => {
                self.u8(op::FILL_RECT);
                self.floats(r);
                self.u32(*c);
            }
            Op::FillRrect(r, radius, c) => {
                self.u8(op::FILL_RRECT);
                self.floats(r);
                self.f32(*radius);
                self.u32(*c);
            }
            Op::StrokeRrect(r, radius, width, c) | Op::StrokeRrectDashed(r, radius, width, c) => {
                self.u8(if matches!(o, Op::StrokeRrect(..)) { op::STROKE_RRECT } else { op::STROKE_RRECT_DASHED });
                self.floats(r);
                self.floats(&[*radius, *width]);
                self.u32(*c);
            }
            Op::Line(from, to, width, c, round) => {
                self.u8(op::LINE);
                self.floats(from);
                self.floats(to);
                self.f32(*width);
                self.u32(*c);
                self.u8(u8::from(*round));
            }
            Op::FillCircle(center, radius, c) => {
                self.u8(op::FILL_CIRCLE);
                self.floats(center);
                self.f32(*radius);
                self.u32(*c);
            }
            Op::StrokeCircle(center, radius, width, c) => {
                self.u8(op::STROKE_CIRCLE);
                self.floats(center);
                self.floats(&[*radius, *width]);
                self.u32(*c);
            }
            Op::StrokePath(width, c, segs) => {
                self.u8(op::STROKE_PATH);
                self.f32(*width);
                self.u32(*c);
                self.segs(segs);
            }
            Op::Triangle(points, c) => {
                self.u8(op::TRIANGLE);
                self.floats(points);
                self.u32(*c);
            }
            Op::ShadowLayer(r, radius, rgb, alpha) => {
                self.u8(op::SHADOW_LAYER);
                self.floats(r);
                self.f32(*radius);
                self.u32(*rgb);
                self.f32(*alpha);
            }
            Op::PushClip(r) => {
                self.u8(op::PUSH_CLIP);
                self.floats(r);
            }
            Op::PopClip => self.u8(op::POP_CLIP),
            Op::Text { at, color, size, weight, tracking, flags, text } => {
                self.u8(op::TEXT);
                self.floats(at);
                self.u32(*color);
                self.f32(*size);
                self.u16(*weight);
                self.f32(*tracking);
                self.u8(*flags);
                self.str(text);
            }
        }
    }
}

/// Reads a buffer.
pub struct Reader<'a>(pub &'a [u8]);

/// Why a buffer was refused.
pub const TRUNCATED: &str = "the buffer ends early";

impl<'a> Reader<'a> {
    pub fn take(&mut self, n: usize) -> Result<&'a [u8], &'static str> {
        if self.0.len() < n {
            return Err(TRUNCATED);
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }

    pub fn u8(&mut self) -> Result<u8, &'static str> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16, &'static str> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub fn u32(&mut self) -> Result<u32, &'static str> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn f32(&mut self) -> Result<f32, &'static str> {
        Ok(f32::from_bits(self.u32()?))
    }

    pub fn floats<const N: usize>(&mut self) -> Result<[f32; N], &'static str> {
        let mut out = [0.0; N];
        for v in &mut out {
            *v = self.f32()?;
        }
        Ok(out)
    }

    pub fn str(&mut self) -> Result<String, &'static str> {
        let len = usize::from(self.u16()?);
        String::from_utf8(self.take(len)?.to_vec()).map_err(|_| "a string is not UTF-8")
    }

    pub fn segs(&mut self) -> Result<Vec<Seg>, &'static str> {
        let count = usize::from(self.u16()?);
        let mut out = Vec::with_capacity(count.min(self.0.len()));
        for _ in 0..count {
            out.push(match self.u8()? {
                seg::MOVE => {
                    let [x, y] = self.floats()?;
                    Seg::Move(x, y)
                }
                seg::LINE => {
                    let [x, y] = self.floats()?;
                    Seg::Line(x, y)
                }
                seg::CUBIC => {
                    let [x1, y1, x2, y2, x, y] = self.floats()?;
                    Seg::Cubic(x1, y1, x2, y2, x, y)
                }
                seg::CLOSE => Seg::Close,
                _ => return Err("a path step this build does not know"),
            });
        }
        Ok(out)
    }
}

/// The operations of a frame's buffer, which must start with [`FRAME`].
pub fn decode(bytes: &[u8]) -> Result<Vec<Op>, &'static str> {
    let mut r = Reader(bytes);
    if r.u8()? != FRAME {
        return Err("not a frame");
    }
    let mut out = Vec::new();
    while !r.0.is_empty() {
        out.push(match r.u8()? {
            op::CLEAR => Op::Clear(r.u32()?),
            op::FILL_RECT => Op::FillRect(r.floats()?, r.u32()?),
            op::FILL_RRECT => Op::FillRrect(r.floats()?, r.f32()?, r.u32()?),
            op::STROKE_RRECT => Op::StrokeRrect(r.floats()?, r.f32()?, r.f32()?, r.u32()?),
            op::STROKE_RRECT_DASHED => Op::StrokeRrectDashed(r.floats()?, r.f32()?, r.f32()?, r.u32()?),
            op::LINE => Op::Line(r.floats()?, r.floats()?, r.f32()?, r.u32()?, r.u8()? != 0),
            op::FILL_CIRCLE => Op::FillCircle(r.floats()?, r.f32()?, r.u32()?),
            op::STROKE_CIRCLE => Op::StrokeCircle(r.floats()?, r.f32()?, r.f32()?, r.u32()?),
            op::STROKE_PATH => Op::StrokePath(r.f32()?, r.u32()?, r.segs()?),
            op::TRIANGLE => Op::Triangle(r.floats()?, r.u32()?),
            op::SHADOW_LAYER => Op::ShadowLayer(r.floats()?, r.f32()?, r.u32()?, r.f32()?),
            op::PUSH_CLIP => Op::PushClip(r.floats()?),
            op::POP_CLIP => Op::PopClip,
            op::TEXT => Op::Text {
                at: r.floats()?,
                color: r.u32()?,
                size: r.f32()?,
                weight: r.u16()?,
                tracking: r.f32()?,
                flags: r.u8()?,
                text: r.str()?,
            },
            _ => return Err("an operation this build does not know"),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_of_each() -> Vec<Op> {
        vec![
            Op::Clear(0xFFEE_F0F3),
            Op::FillRect([1.0, 2.0, 3.0, 4.0], 0xFF00_00FF),
            Op::FillRrect([1.5, 2.5, 30.0, 40.0], 8.0, 0x8012_3456),
            Op::StrokeRrect([0.0, 0.0, 10.0, 10.0], 3.0, 2.0, 0xFF33_46D3),
            Op::StrokeRrectDashed([5.0, 5.0, 100.0, 50.0], 12.0, 1.5, 0xFF57_5E6B),
            Op::Line([0.0, 1.0], [10.0, 11.0], 2.5, 0xFF17_1A21, true),
            Op::FillCircle([8.0, 8.0], 5.0, 0xFF1E_7B4F),
            Op::StrokeCircle([8.0, 8.0], 5.0, 1.75, 0xFF1E_7B4F),
            Op::StrokePath(
                1.75,
                0xFF17_1A21,
                vec![
                    Seg::Move(4.0, 4.0),
                    Seg::Line(20.0, 4.0),
                    Seg::Cubic(21.0, 4.0, 22.0, 5.0, 22.0, 6.0),
                    Seg::Close,
                ],
            ),
            Op::Triangle([0.0, 0.0, 10.0, 5.0, 0.0, 10.0], 0xFF33_46D3),
            Op::ShadowLayer([10.0, 10.0, 200.0, 100.0], 12.0, 0x0017_1A21, 0.012),
            Op::PushClip([0.0, 0.0, 300.0, 200.0]),
            Op::PopClip,
            Op::Text {
                at: [16.0, 40.0],
                color: 0xFF17_1A21,
                size: 36.75,
                weight: 600,
                tracking: -0.015,
                flags: flag::MONO | flag::ITALIC,
                text: "Você controla o MacBook".into(),
            },
        ]
    }

    #[test]
    fn every_operation_reads_back_as_it_was_written() {
        let ops = one_of_each();
        let mut w = Writer::default();
        w.u8(FRAME);
        for o in &ops {
            w.push(o);
        }
        assert_eq!(decode(&w.0).unwrap(), ops);
        // Any shorter buffer ends early, or ends between operations and holds fewer of them.
        for cut in 1..w.0.len() {
            match decode(&w.0[..cut]) {
                Ok(fewer) => assert!(fewer.len() < ops.len(), "cut at {cut}"),
                Err(why) => assert_eq!(why, TRUNCATED, "cut at {cut}"),
            }
        }
        assert_eq!(decode(&[NEEDS]), Err("not a frame"));
        assert_eq!(decode(&[FRAME, 200]), Err("an operation this build does not know"));
        let mut bad_step = Writer::default();
        bad_step.u8(FRAME);
        bad_step.u8(op::STROKE_PATH);
        bad_step.f32(1.0);
        bad_step.u32(0);
        bad_step.u16(1);
        bad_step.u8(9);
        assert_eq!(decode(&bad_step.0), Err("a path step this build does not know"));
    }

    #[test]
    fn a_long_string_is_cut_at_a_character_and_a_colour_is_argb() {
        let mut w = Writer::default();
        w.str(&"é".repeat(40_000));
        let mut r = Reader(&w.0);
        let back = r.str().unwrap();
        assert_eq!((back.len(), back.chars().all(|c| c == 'é')), (65_534, true));
        assert_eq!(argb(Color { r: 0x33, g: 0x46, b: 0xD3, a: 0x80 }), 0x8033_46D3);
    }
}
