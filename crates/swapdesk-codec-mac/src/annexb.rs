//! Annex B helpers for the VideoToolbox decoder: NAL unit scan, parameter-set extraction, the Annex B to AVCC
//! conversion VideoToolbox wants, and a splitter of a raw stream into access units (tests and tools). Pure code, no OS
//! calls: it builds and is unit-tested on every host.
//!
//! Format facts (ITU-T H.264 Annex B and ISO/IEC 14496-15): an Annex B NAL unit follows a 3- or 4-byte start code
//! (`00 00 01`, `00 00 00 01`); VideoToolbox decodes AVCC, where each NAL unit follows a big-endian length field, and
//! gets the SPS/PPS through its format description instead of in the samples.

use std::ops::Range;

pub const NAL_SLICE: u8 = 1;
pub const NAL_IDR: u8 = 5;
pub const NAL_SEI: u8 = 6;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
pub const NAL_AUD: u8 = 9;
pub const NAL_FILLER: u8 = 12;

/// Bytes of the length field of the AVCC samples this crate builds (and `nal_unit_header_length` of the format
/// description).
pub const AVCC_LENGTH_SIZE: usize = 4;

/// One NAL unit: `start` is the first byte of its start code, `payload` the offset of the NAL header byte, `end` the
/// offset after its last byte (trailing zero bytes excluded).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Nal {
    pub start: usize,
    pub payload: usize,
    pub end: usize,
    pub kind: u8,
}

impl Nal {
    /// The NAL unit without its start code, header byte included.
    pub fn bytes<'a>(&self, data: &'a [u8]) -> &'a [u8] {
        &data[self.payload..self.end]
    }
}

/// Splits an Annex B buffer at its 3- and 4-byte start codes. Bytes before the first start code are ignored.
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
        // A NAL unit never ends in a zero byte (the RBSP trailing bits end in a one, a cabac_zero_word ends in 03):
        // trailing zeros are padding before the next start code.
        while end > payload && data[end - 1] == 0 {
            end -= 1;
        }
        if end <= payload {
            continue;
        }
        out.push(Nal { start, payload, end, kind: data[payload] & 0x1F });
    }
    out
}

/// A keyframe starts a new coded video sequence: it holds an IDR slice.
pub fn is_idr(data: &[u8]) -> bool {
    scan(data).iter().any(|n| n.kind == NAL_IDR)
}

/// The SPS and PPS NAL units of an access unit, each without start code, in stream order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParamSets {
    pub sps: Vec<Vec<u8>>,
    pub pps: Vec<Vec<u8>>,
}

impl ParamSets {
    /// The sets in the order the format description wants: every SPS, then every PPS.
    pub fn all(&self) -> impl Iterator<Item = &[u8]> {
        self.sps.iter().chain(self.pps.iter()).map(Vec::as_slice)
    }
}

/// The parameter sets of `data`; `None` unless it holds at least one SPS and one PPS.
pub fn parameter_sets(data: &[u8]) -> Option<ParamSets> {
    let mut sets = ParamSets::default();
    for nal in scan(data) {
        match nal.kind {
            NAL_SPS => sets.sps.push(nal.bytes(data).to_vec()),
            NAL_PPS => sets.pps.push(nal.bytes(data).to_vec()),
            _ => {}
        }
    }
    (!sets.sps.is_empty() && !sets.pps.is_empty()).then_some(sets)
}

/// Annex B to AVCC: every NAL unit except SPS, PPS, access unit delimiters and filler data, each behind a 4-byte
/// big-endian length. Empty when nothing is left to decode.
pub fn to_avcc(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for nal in scan(data) {
        if matches!(nal.kind, NAL_SPS | NAL_PPS | NAL_AUD | NAL_FILLER) {
            continue;
        }
        let body = nal.bytes(data);
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(body);
    }
    out
}

/// True when the access unit holds a coded slice (a picture to decode).
pub fn has_picture(data: &[u8]) -> bool {
    scan(data).iter().any(|n| (NAL_SLICE..=NAL_IDR).contains(&n.kind))
}

/// Splits a raw Annex B stream into access units, one picture each. A unit starts at the first NAL unit that precedes
/// a slice with `first_mb_in_slice == 0` (SPS, PPS, SEI and delimiters travel with the picture they come before).
/// Streams with several slices per picture work, since only the first slice has the macroblock address 0.
pub fn access_units(data: &[u8]) -> Vec<Range<usize>> {
    let mut units = Vec::new();
    let mut current: Option<usize> = None;
    let mut seen_picture = false;
    // Start of the non-picture NAL units seen since the last slice.
    let mut lead: Option<usize> = None;
    for nal in scan(data) {
        if (NAL_SLICE..=NAL_IDR).contains(&nal.kind) {
            let first_slice = data.get(nal.payload + 1).is_some_and(|b| b & 0x80 != 0);
            if first_slice && seen_picture {
                let boundary = lead.unwrap_or(nal.start);
                if let Some(start) = current {
                    units.push(start..boundary);
                }
                current = Some(boundary);
            }
            current.get_or_insert(lead.unwrap_or(nal.start));
            seen_picture = true;
            lead = None;
        } else {
            lead.get_or_insert(nal.start);
        }
    }
    if let Some(start) = current {
        units.push(start..data.len());
    }
    units
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: [u8; 4] = [0x67, 0x42, 0xC0, 0x1E];
    const PPS: [u8; 2] = [0x68, 0xCE];
    const IDR: [u8; 4] = [0x65, 0x88, 0x84, 0x00];
    const P: [u8; 3] = [0x41, 0x9A, 0x24];

    fn join(parts: &[(&[u8], &[u8])]) -> Vec<u8> {
        let mut v = Vec::new();
        for (code, body) in parts {
            v.extend_from_slice(code);
            v.extend_from_slice(body);
        }
        v
    }

    const SC4: &[u8] = &[0, 0, 0, 1];
    const SC3: &[u8] = &[0, 0, 1];

    #[test]
    fn scan_finds_three_and_four_byte_start_codes_and_trims_padding() {
        let data = join(&[(SC4, &SPS), (SC3, &PPS), (SC4, &IDR)]);
        let nals = scan(&data);
        assert_eq!(nals.iter().map(|n| n.kind).collect::<Vec<_>>(), [NAL_SPS, NAL_PPS, NAL_IDR]);
        assert_eq!(nals[0].start, 0);
        assert_eq!(nals[0].bytes(&data), SPS);
        assert_eq!(nals[1].start, 4 + SPS.len());
        assert_eq!(nals[1].bytes(&data), PPS);
        // The IDR's trailing zero byte is padding, not part of the unit.
        assert_eq!(nals[2].bytes(&data), &IDR[..3]);
    }

    #[test]
    fn scan_ignores_junk_before_the_first_start_code_and_empty_input() {
        assert!(scan(&[]).is_empty());
        assert!(scan(&[1, 2, 3, 4]).is_empty());
        let data = join(&[(&[9, 9], &[]), (SC3, &P)]);
        let nals = scan(&data);
        assert_eq!(nals.len(), 1);
        assert_eq!(nals[0].kind, NAL_SLICE);
    }

    #[test]
    fn parameter_sets_need_an_sps_and_a_pps() {
        let both = join(&[(SC4, &SPS), (SC4, &PPS), (SC4, &IDR)]);
        let sets = parameter_sets(&both).expect("sets");
        assert_eq!(sets.sps, [SPS.to_vec()]);
        assert_eq!(sets.pps, [PPS.to_vec()]);
        assert_eq!(sets.all().collect::<Vec<_>>(), [&SPS[..], &PPS[..]]);
        assert!(parameter_sets(&join(&[(SC4, &SPS), (SC4, &IDR)])).is_none());
        assert!(parameter_sets(&join(&[(SC4, &P)])).is_none());
    }

    #[test]
    fn to_avcc_drops_parameter_sets_and_prefixes_lengths() {
        let data = join(&[(SC4, &[0x09, 0xF0]), (SC4, &SPS), (SC4, &PPS), (SC3, &IDR[..3])]);
        let avcc = to_avcc(&data);
        assert_eq!(avcc, [0, 0, 0, 3, 0x65, 0x88, 0x84]);
        let p = to_avcc(&join(&[(SC4, &P)]));
        assert_eq!(p, [0, 0, 0, 3, 0x41, 0x9A, 0x24]);
        assert!(to_avcc(&join(&[(SC4, &SPS), (SC4, &PPS)])).is_empty());
    }

    #[test]
    fn idr_and_picture_detection() {
        assert!(is_idr(&join(&[(SC4, &SPS), (SC4, &IDR)])));
        assert!(!is_idr(&join(&[(SC4, &P)])));
        assert!(has_picture(&join(&[(SC4, &P)])));
        assert!(!has_picture(&join(&[(SC4, &SPS), (SC4, &PPS)])));
    }

    #[test]
    fn access_units_split_before_the_parameter_sets_of_the_next_picture() {
        // IDR (first_mb 0: second byte has the top bit set), P, SPS+PPS+IDR, P with a second slice.
        let idr: [u8; 3] = [0x65, 0x88, 0x84];
        let p1: [u8; 3] = [0x41, 0x9A, 0x24];
        let p2_second_slice: [u8; 3] = [0x41, 0x1A, 0x24];
        let data = join(&[
            (SC4, &SPS),
            (SC4, &PPS),
            (SC4, &idr),
            (SC4, &p1),
            (SC4, &SPS),
            (SC4, &PPS),
            (SC4, &idr),
            (SC4, &p1),
            (SC4, &p2_second_slice),
        ]);
        let units = access_units(&data);
        assert_eq!(units.len(), 4);
        assert_eq!(units[0].start, 0);
        assert!(is_idr(&data[units[0].clone()]) && parameter_sets(&data[units[0].clone()]).is_some());
        assert!(!is_idr(&data[units[1].clone()]) && !has_parameter_sets(&data[units[1].clone()]));
        assert!(is_idr(&data[units[2].clone()]) && parameter_sets(&data[units[2].clone()]).is_some());
        // The second slice of the last picture stays in its unit.
        assert_eq!(scan(&data[units[3].clone()]).len(), 2);
        assert_eq!(units.last().map(|u| u.end), Some(data.len()));
        for pair in units.windows(2) {
            assert_eq!(pair[0].end, pair[1].start);
        }
    }

    fn has_parameter_sets(data: &[u8]) -> bool {
        scan(data).iter().any(|n| matches!(n.kind, NAL_SPS | NAL_PPS))
    }

    #[test]
    fn access_units_of_nothing_is_nothing() {
        assert!(access_units(&[]).is_empty());
        assert!(access_units(&join(&[(SC4, &SPS), (SC4, &PPS)])).is_empty());
    }
}
