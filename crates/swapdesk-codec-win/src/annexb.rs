//! Annex B helpers: NAL unit scan, parameter-set extraction, SPS fields. Pure code, no OS calls.

pub const NAL_SLICE: u8 = 1;
pub const NAL_IDR: u8 = 5;
pub const NAL_SEI: u8 = 6;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;

/// One NAL unit: `start` is the first byte of its start code, `payload` the offset of the NAL header byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Nal {
    pub start: usize,
    pub payload: usize,
    pub end: usize,
    pub kind: u8,
}

/// Splits an Annex B buffer at its 3- and 4-byte start codes.
pub fn scan(data: &[u8]) -> Vec<Nal> {
    let mut found: Vec<(usize, usize)> = Vec::new(); // (start code offset, payload offset)
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let start = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
            found.push((start, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(found.len());
    for (n, &(start, payload)) in found.iter().enumerate() {
        let mut end = found.get(n + 1).map_or(data.len(), |next| next.0);
        // Trailing zero bytes belong to the next start code or are padding.
        while end > payload && data[end - 1] == 0 && n + 1 == found.len() {
            end -= 1;
        }
        let Some(&header) = data.get(payload) else { continue };
        out.push(Nal { start, payload, end, kind: header & 0x1F });
    }
    out
}

/// True when any NAL unit has this type.
pub fn contains(data: &[u8], kind: u8) -> bool {
    scan(data).iter().any(|n| n.kind == kind)
}

/// The SPS and PPS NAL units of `data` (with their start codes), in stream order. Empty when there are none.
pub fn parameter_sets(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in scan(data) {
        if nal.kind == NAL_SPS || nal.kind == NAL_PPS {
            out.extend_from_slice(&data[nal.start..nal.end]);
        }
    }
    out
}

/// A keyframe starts a new coded video sequence: it holds an IDR slice.
pub fn is_idr(data: &[u8]) -> bool {
    contains(data, NAL_IDR)
}

/// `profile_idc` and `level_idc` of the first SPS.
pub fn sps_profile_level(data: &[u8]) -> Option<(u8, u8)> {
    let nal = scan(data).into_iter().find(|n| n.kind == NAL_SPS)?;
    let body = data.get(nal.payload + 1..nal.end)?;
    // profile_idc, constraint flags, level_idc
    Some((*body.first()?, *body.get(2)?))
}

/// Removes emulation prevention bytes (00 00 03 -> 00 00) from an RBSP.
fn unescape(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len());
    let mut zeros = 0;
    for &b in body {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos / 8)?;
        let v = u32::from(byte >> (7 - self.pos % 8) & 1);
        self.pos += 1;
        Some(v)
    }
    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0;
        for _ in 0..n {
            v = v << 1 | self.bit()?;
        }
        Some(v)
    }
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + self.bits(zeros)?)
    }
    fn se(&mut self) -> Option<i32> {
        let k = self.ue()?;
        let magnitude = k.div_ceil(2) as i32;
        Some(if k % 2 == 1 { magnitude } else { -magnitude })
    }
}

/// What the first SPS says about the picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpsInfo {
    pub profile_idc: u8,
    pub level_idc: u8,
    pub chroma_format_idc: u32,
    /// Displayed size after cropping.
    pub width: u32,
    pub height: u32,
    /// `max_num_reorder_frames` when the VUI says so; `None` when absent.
    pub num_reorder_frames: Option<u32>,
}

/// Parses the first SPS (profile, level, chroma format, cropped size, reorder depth).
pub fn parse_sps(data: &[u8]) -> Option<SpsInfo> {
    let nal = scan(data).into_iter().find(|n| n.kind == NAL_SPS)?;
    let rbsp = unescape(data.get(nal.payload + 1..nal.end)?);
    let mut b = Bits { data: &rbsp, pos: 0 };
    let profile_idc = b.bits(8)? as u8;
    b.bits(8)?; // constraint flags + reserved
    let level_idc = b.bits(8)? as u8;
    b.ue()?; // sps id
    let mut chroma_format_idc = 1;
    if matches!(profile_idc, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        chroma_format_idc = b.ue()?;
        if chroma_format_idc == 3 {
            b.bit()?;
        }
        b.ue()?; // bit depth luma
        b.ue()?; // bit depth chroma
        b.bit()?; // transform bypass
        if b.bit()? == 1 {
            let lists = if chroma_format_idc != 3 { 8 } else { 12 };
            for i in 0..lists {
                if b.bit()? == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i32, 8i32);
                    for _ in 0..size {
                        if next != 0 {
                            next = (last + b.se()? + 256) % 256;
                        }
                        if next != 0 {
                            last = next;
                        }
                    }
                }
            }
        }
    }
    b.ue()?; // log2_max_frame_num_minus4
    let poc_type = b.ue()?;
    if poc_type == 0 {
        b.ue()?;
    } else if poc_type == 1 {
        b.bit()?;
        b.se()?;
        b.se()?;
        let n = b.ue()?;
        for _ in 0..n {
            b.se()?;
        }
    }
    b.ue()?; // max_num_ref_frames
    b.bit()?; // gaps allowed
    let mbs_w = b.ue()? + 1;
    let map_h = b.ue()? + 1;
    let frame_mbs_only = b.bit()?;
    if frame_mbs_only == 0 {
        b.bit()?;
    }
    b.bit()?; // direct 8x8
    let (mut cl, mut cr, mut ct, mut cb) = (0, 0, 0, 0);
    if b.bit()? == 1 {
        cl = b.ue()?;
        cr = b.ue()?;
        ct = b.ue()?;
        cb = b.ue()?;
    }
    let (sub_w, sub_h) = match chroma_format_idc {
        0 => (1, 1),
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    let crop_unit_y = sub_h * (2 - frame_mbs_only);
    let width = mbs_w * 16 - (cl + cr) * sub_w;
    let height = (2 - frame_mbs_only) * map_h * 16 - (ct + cb) * crop_unit_y;
    let mut num_reorder_frames = None;
    if b.bit()? == 1 {
        // VUI
        if b.bit()? == 1 && b.bits(8)? == 255 {
            b.bits(32)?;
        }
        if b.bit()? == 1 {
            b.bit()?;
        }
        if b.bit()? == 1 {
            b.bits(4)?;
            if b.bit()? == 1 {
                b.bits(24)?;
            }
        }
        if b.bit()? == 1 {
            b.ue()?;
            b.ue()?;
        }
        if b.bit()? == 1 {
            b.bits(32)?;
            b.bits(32)?;
            b.bit()?;
        }
        let nal_hrd = b.bit()?;
        if nal_hrd == 1 {
            skip_hrd(&mut b)?;
        }
        let vcl_hrd = b.bit()?;
        if vcl_hrd == 1 {
            skip_hrd(&mut b)?;
        }
        if nal_hrd == 1 || vcl_hrd == 1 {
            b.bit()?;
        }
        b.bit()?; // pic_struct_present
        if b.bit()? == 1 {
            b.bit()?;
            b.ue()?;
            b.ue()?;
            b.ue()?;
            b.ue()?;
            num_reorder_frames = Some(b.ue()?);
        }
    }
    Some(SpsInfo { profile_idc, level_idc, chroma_format_idc, width, height, num_reorder_frames })
}

fn skip_hrd(b: &mut Bits<'_>) -> Option<()> {
    let cpb = b.ue()? + 1;
    b.bits(8)?;
    for _ in 0..cpb {
        b.ue()?;
        b.ue()?;
        b.bit()?;
    }
    b.bits(20)?;
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct BitWriter {
        bytes: Vec<u8>,
        used: u32,
    }

    impl BitWriter {
        fn bits(&mut self, value: u32, n: u32) {
            for i in (0..n).rev() {
                if self.used.is_multiple_of(8) {
                    self.bytes.push(0);
                }
                let bit = (value >> i & 1) as u8;
                *self.bytes.last_mut().unwrap() |= bit << (7 - self.used % 8);
                self.used += 1;
            }
        }
        fn ue(&mut self, v: u32) {
            let x = v + 1;
            let len = 32 - x.leading_zeros();
            self.bits(0, len - 1);
            self.bits(x, len);
        }
    }

    /// An SPS for a High profile level 4.2 picture, built bit by bit.
    fn build_sps(width: u32, height: u32, crop_bottom: u32, reorder: Option<u32>) -> Vec<u8> {
        let mut w = BitWriter { bytes: Vec::new(), used: 0 };
        w.bits(100, 8);
        w.bits(0, 8);
        w.bits(42, 8);
        w.ue(0); // sps id
        w.ue(1); // chroma_format_idc 4:2:0
        w.ue(0);
        w.ue(0);
        w.bits(0, 1); // transform bypass
        w.bits(0, 1); // scaling matrix
        w.ue(0); // log2_max_frame_num_minus4
        w.ue(2); // poc type 2
        w.ue(1); // max_num_ref_frames
        w.bits(0, 1);
        w.ue(width.div_ceil(16) - 1);
        w.ue(height.div_ceil(16) - 1);
        w.bits(1, 1); // frame_mbs_only
        w.bits(1, 1); // direct 8x8
        if crop_bottom > 0 {
            w.bits(1, 1);
            w.ue(0);
            w.ue(0);
            w.ue(0);
            w.ue(crop_bottom / 2);
        } else {
            w.bits(0, 1);
        }
        match reorder {
            Some(n) => {
                w.bits(1, 1); // vui
                w.bits(0, 6); // aspect, overscan, video signal, chroma loc, timing, nal hrd
                w.bits(0, 1); // vcl hrd
                w.bits(0, 1); // pic_struct
                w.bits(1, 1); // bitstream_restriction
                w.bits(1, 1);
                w.ue(0);
                w.ue(0);
                w.ue(16);
                w.ue(16);
                w.ue(n);
                w.ue(1);
            }
            None => w.bits(0, 1),
        }
        w.bits(1, 1); // rbsp stop bit
        let mut out = vec![0, 0, 0, 1, 0x67];
        out.extend_from_slice(&w.bytes);
        out
    }

    #[test]
    fn scan_finds_three_and_four_byte_start_codes() {
        let data = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9];
        let nals = scan(&data);
        assert_eq!(nals.iter().map(|n| n.kind).collect::<Vec<_>>(), vec![7, 8, 5]);
        assert_eq!(nals[0].start, 0);
        assert_eq!(nals[1].start, 7);
        assert_eq!(nals[2].start, 12);
        assert!(is_idr(&data));
        assert!(!is_idr(&data[..12]));
    }

    #[test]
    fn parameter_sets_keep_start_codes() {
        let mut data = Vec::new();
        data.extend_from_slice(&[0, 0, 0, 1, 0x67, 1, 2]);
        data.extend_from_slice(&[0, 0, 0, 1, 0x68, 3]);
        data.extend_from_slice(&[0, 0, 1, 0x65, 8, 8]);
        let ps = parameter_sets(&data);
        assert_eq!(ps, vec![0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3]);
    }

    #[test]
    fn sps_fields() {
        let sps = build_sps(1920, 1080, 8, Some(0));
        assert_eq!(sps_profile_level(&sps), Some((100, 42)));
        let info = parse_sps(&sps).expect("sps parses");
        assert_eq!((info.profile_idc, info.level_idc, info.chroma_format_idc), (100, 42, 1));
        assert_eq!((info.width, info.height), (1920, 1080));
        assert_eq!(info.num_reorder_frames, Some(0));
        let plain = parse_sps(&build_sps(1280, 720, 0, None)).expect("sps parses");
        assert_eq!((plain.width, plain.height, plain.num_reorder_frames), (1280, 720, None));
    }

    #[test]
    fn exp_golomb() {
        let mut b = Bits { data: &[0b1010_0110, 0b0100_0000], pos: 0 };
        assert_eq!(b.ue(), Some(0)); // 1
        assert_eq!(b.ue(), Some(1)); // 010
        assert_eq!(b.ue(), Some(2)); // 011
        assert_eq!(b.ue(), Some(3)); // 00100
    }
}
