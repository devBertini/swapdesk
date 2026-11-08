//! Audio datagrams (PROTOCOL.md 7.2): the 16-byte `AudioHeader` of `swapdesk-proto`, then one Opus packet.
//! A muted frame is a header with the MUTED flag and no payload.

use swapdesk_proto::{AudioHeader, CodecError, Wire};

use crate::{FRAME_MS, HEADER_LEN, MAX_DATAGRAM, MAX_OPUS_PACKET};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PacketError {
    /// Fewer than 16 bytes.
    TooShort(usize),
    /// More than [`MAX_DATAGRAM`] bytes: rejected before anything is parsed.
    TooLong(usize),
    /// The header is not an audio header (kind or flag bits).
    Header(CodecError),
    /// `frame_ms` is not 10.
    FrameMs(u16),
    /// A frame that is not muted carries no Opus packet.
    EmptyPayload,
    /// A muted frame carries a payload.
    PayloadWhileMuted,
    /// The Opus packet does not fit one datagram (sender side).
    PayloadTooLarge(usize),
}

impl core::fmt::Display for PacketError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TooShort(n) => write!(f, "datagram of {n} bytes is shorter than the header"),
            Self::TooLong(n) => write!(f, "datagram of {n} bytes exceeds {MAX_DATAGRAM}"),
            Self::Header(e) => write!(f, "bad header: {e:?}"),
            Self::FrameMs(ms) => write!(f, "frame_ms {ms} is not {FRAME_MS}"),
            Self::EmptyPayload => write!(f, "unmuted frame without an Opus packet"),
            Self::PayloadWhileMuted => write!(f, "muted frame with a payload"),
            Self::PayloadTooLarge(n) => {
                write!(f, "Opus packet of {n} bytes exceeds {MAX_OPUS_PACKET}")
            }
        }
    }
}

impl std::error::Error for PacketError {}

/// Header and payload as one datagram.
pub fn build_datagram(header: &AudioHeader, payload: &[u8]) -> Result<Vec<u8>, PacketError> {
    if payload.len() > MAX_OPUS_PACKET {
        return Err(PacketError::PayloadTooLarge(payload.len()));
    }
    if header.muted && !payload.is_empty() {
        return Err(PacketError::PayloadWhileMuted);
    }
    if !header.muted && payload.is_empty() {
        return Err(PacketError::EmptyPayload);
    }
    let mut out = header.encode().map_err(PacketError::Header)?;
    debug_assert_eq!(out.len(), HEADER_LEN);
    out.extend_from_slice(payload);
    Ok(out)
}

/// Splits a received datagram; the length is checked before the header is read.
pub fn parse_datagram(bytes: &[u8]) -> Result<(AudioHeader, &[u8]), PacketError> {
    if bytes.len() > MAX_DATAGRAM {
        return Err(PacketError::TooLong(bytes.len()));
    }
    if bytes.len() < HEADER_LEN {
        return Err(PacketError::TooShort(bytes.len()));
    }
    let (head, payload) = bytes.split_at(HEADER_LEN);
    let header = AudioHeader::decode(head).map_err(PacketError::Header)?;
    if header.frame_ms != FRAME_MS {
        return Err(PacketError::FrameMs(header.frame_ms));
    }
    if header.muted && !payload.is_empty() {
        return Err(PacketError::PayloadWhileMuted);
    }
    if !header.muted && payload.is_empty() {
        return Err(PacketError::EmptyPayload);
    }
    Ok((header, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(muted: bool) -> AudioHeader {
        AudioHeader { discontinuity: false, muted, frame_ms: FRAME_MS, seq: 0xFFFF_FFFF, capture_us: 123_456_789 }
    }

    #[test]
    fn round_trip_keeps_header_and_payload() {
        let payload = [7u8; 160];
        let d = build_datagram(&header(false), &payload).unwrap();
        assert_eq!(d.len(), 176);
        assert_eq!(d[0], 0x01);
        let (h, p) = parse_datagram(&d).unwrap();
        assert_eq!(h, header(false));
        assert_eq!(p, payload);
    }

    #[test]
    fn muted_frame_is_header_only() {
        let d = build_datagram(&header(true), &[]).unwrap();
        assert_eq!(d.len(), HEADER_LEN);
        let (h, p) = parse_datagram(&d).unwrap();
        assert!(h.muted && p.is_empty());
        assert_eq!(build_datagram(&header(true), &[1]), Err(PacketError::PayloadWhileMuted));
    }

    #[test]
    fn limits_are_enforced_on_both_sides() {
        // Exactly the largest packet fits; one more byte does not.
        let max = vec![1u8; MAX_OPUS_PACKET];
        let d = build_datagram(&header(false), &max).unwrap();
        assert_eq!(d.len(), MAX_DATAGRAM);
        assert!(parse_datagram(&d).is_ok());
        assert_eq!(
            build_datagram(&header(false), &vec![1u8; MAX_OPUS_PACKET + 1]),
            Err(PacketError::PayloadTooLarge(MAX_OPUS_PACKET + 1))
        );
        let mut big = d.clone();
        big.push(0);
        assert_eq!(parse_datagram(&big), Err(PacketError::TooLong(MAX_DATAGRAM + 1)));
        assert_eq!(parse_datagram(&d[..HEADER_LEN - 1]), Err(PacketError::TooShort(HEADER_LEN - 1)));
        assert_eq!(parse_datagram(&[]), Err(PacketError::TooShort(0)));
        assert_eq!(build_datagram(&header(false), &[]), Err(PacketError::EmptyPayload));
        let header_only = build_datagram(&header(true), &[]).unwrap();
        // The same bytes without the MUTED flag are an empty unmuted frame.
        let mut unmuted = header_only.clone();
        unmuted[1] = 0;
        assert_eq!(parse_datagram(&unmuted), Err(PacketError::EmptyPayload));
    }

    #[test]
    fn other_frame_lengths_and_kinds_are_refused() {
        let h = AudioHeader { frame_ms: 20, ..header(false) };
        let mut d = h.encode().unwrap();
        d.extend_from_slice(&[1; 10]);
        assert_eq!(parse_datagram(&d), Err(PacketError::FrameMs(20)));
        let mut cursor = build_datagram(&header(false), &[1; 10]).unwrap();
        cursor[0] = 0x02;
        assert!(matches!(parse_datagram(&cursor), Err(PacketError::Header(_))));
        let mut flags = build_datagram(&header(false), &[1; 10]).unwrap();
        flags[1] = 0b100;
        assert!(matches!(parse_datagram(&flags), Err(PacketError::Header(_))));
    }

    #[test]
    fn garbage_never_panics() {
        let mut x = 0x1234_5678u32;
        for len in 0..300usize {
            let bytes: Vec<u8> = (0..len)
                .map(|_| {
                    x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (x >> 24) as u8
                })
                .collect();
            let _ = parse_datagram(&bytes);
        }
    }
}
