//! The hand-written codec of PROTOCOL.md 9.2: QUIC varints (minimal only), little-endian fixed integers,
//! strict booleans, bounded strings and vectors, and a [`Reader`] that never reserves memory from a
//! peer-declared length before checking it against its limit and against the bytes that are left.

use core::fmt;

use crate::types::ErrorCode;

/// Largest value a QUIC varint holds (RFC 9000 section 16).
pub const MAX_VARINT: u64 = (1 << 62) - 1;

/// Why bytes were rejected (decoding) or a value could not be written (encoding).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecError {
    /// The input ended before the value did.
    Truncated,
    /// Bytes remain after the value ended.
    TrailingBytes,
    /// A varint used a longer form than its value needs (RFC 9000 allows it, Swapdesk does not).
    NonMinimalVarint,
    /// Encoding only: the value does not fit in 62 bits.
    VarintOutOfRange,
    /// A boolean byte other than 0 or 1.
    InvalidBool,
    /// A string that is not valid UTF-8.
    InvalidUtf8,
    /// A `str(max)` that holds a NUL.
    InteriorNul,
    /// A string length or vector count above its limit.
    OverLimit { what: &'static str, limit: u64 },
    /// A vector count that the bytes left cannot hold, even at the smallest item size.
    CountExceedsInput,
    /// A control frame length above 65 535, a video payload above 8 MiB or a clipboard item above its limit.
    TooLarge { what: &'static str, limit: u64 },
    /// An enum tag, message type, stream kind or option tag this version does not know.
    UnknownTag { what: &'static str, tag: u64 },
    /// A well-formed field with a value the format forbids (reserved bits set, a bad version, a permission set
    /// that breaks "implies").
    InvalidValue(&'static str),
}

impl CodecError {
    /// The error code a peer is told (section 8): only a too-large frame is `TOO_LARGE`, every other
    /// malformed input is a `PROTOCOL_VIOLATION` (section 5.1).
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::TooLarge { .. } => ErrorCode::TooLarge,
            _ => ErrorCode::ProtocolViolation,
        }
    }
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => f.write_str("input ended early"),
            Self::TrailingBytes => f.write_str("bytes left after the value"),
            Self::NonMinimalVarint => f.write_str("non-minimal varint"),
            Self::VarintOutOfRange => f.write_str("value does not fit in a varint"),
            Self::InvalidBool => f.write_str("boolean byte is not 0 or 1"),
            Self::InvalidUtf8 => f.write_str("string is not valid UTF-8"),
            Self::InteriorNul => f.write_str("string holds a NUL"),
            Self::OverLimit { what, limit } => write!(f, "{what} is over its limit of {limit}"),
            Self::CountExceedsInput => f.write_str("declared count cannot fit in the remaining bytes"),
            Self::TooLarge { what, limit } => write!(f, "{what} is over {limit}"),
            Self::UnknownTag { what, tag } => write!(f, "unknown {what} {tag:#x}"),
            Self::InvalidValue(what) => write!(f, "invalid {what}"),
        }
    }
}

impl std::error::Error for CodecError {}

/// Result of every codec operation.
pub type Result<T> = core::result::Result<T, CodecError>;

/// Append-only byte sink with the primitive encoders of section 9.2.
#[derive(Debug, Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    /// A writer that appends to `buf`, keeping its bytes and its capacity: one buffer serves many encodings
    /// (`into_bytes` hands it back).
    pub fn appending(buf: Vec<u8>) -> Self {
        Self { buf }
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    pub fn put_u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn put_u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn put_u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn put_u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn put_i16(&mut self, v: i16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn put_i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn put_bool(&mut self, v: bool) {
        self.buf.push(u8::from(v));
    }

    pub fn put_bytes(&mut self, v: &[u8]) {
        self.buf.extend_from_slice(v);
    }

    /// The minimal QUIC varint of `v`.
    pub fn put_varint(&mut self, v: u64) -> Result<()> {
        if v < 1 << 6 {
            self.buf.push(v as u8);
        } else if v < 1 << 14 {
            self.buf.extend_from_slice(&(0x4000 | v as u16).to_be_bytes());
        } else if v < 1 << 30 {
            self.buf.extend_from_slice(&(0x8000_0000 | v as u32).to_be_bytes());
        } else if v <= MAX_VARINT {
            self.buf.extend_from_slice(&(0xC000_0000_0000_0000 | v).to_be_bytes());
        } else {
            return Err(CodecError::VarintOutOfRange);
        }
        Ok(())
    }

    /// `str(max)`: varint byte length, then the bytes; over `max` bytes or a NUL is an error.
    pub fn put_str(&mut self, s: &str, max: usize) -> Result<()> {
        if s.len() > max {
            return Err(CodecError::OverLimit { what: "string length", limit: max as u64 });
        }
        if s.as_bytes().contains(&0) {
            return Err(CodecError::InteriorNul);
        }
        self.put_varint(s.len() as u64)?;
        self.put_bytes(s.as_bytes());
        Ok(())
    }
}

/// Cursor over a byte slice with the primitive decoders of section 9.2.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Bytes read so far.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Bytes not read yet.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Fails with [`CodecError::TrailingBytes`] unless every byte was read.
    pub fn finish(&self) -> Result<()> {
        if self.remaining() == 0 { Ok(()) } else { Err(CodecError::TrailingBytes) }
    }

    /// The next `n` bytes, borrowed. `n` is checked against the bytes left, so a hostile length costs nothing.
    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.remaining() {
            return Err(CodecError::Truncated);
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    pub fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    pub fn i16(&mut self) -> Result<i16> {
        Ok(i16::from_le_bytes(self.array()?))
    }

    pub fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.array()?))
    }

    pub fn bool(&mut self) -> Result<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(CodecError::InvalidBool),
        }
    }

    /// A QUIC varint in its minimal form only.
    pub fn varint(&mut self) -> Result<u64> {
        let first = *self.buf.get(self.pos).ok_or(CodecError::Truncated)?;
        let len = 1usize << (first >> 6);
        if len > self.remaining() {
            return Err(CodecError::Truncated);
        }
        let mut v = u64::from(first & 0x3F);
        for b in &self.buf[self.pos + 1..self.pos + len] {
            v = (v << 8) | u64::from(*b);
        }
        let smallest = match len {
            1 => 0,
            2 => 1 << 6,
            4 => 1 << 14,
            _ => 1 << 30,
        };
        if v < smallest {
            return Err(CodecError::NonMinimalVarint);
        }
        self.pos += len;
        Ok(v)
    }

    /// `str(max)`: varint byte length ≤ `max`, valid UTF-8, no NUL. At most `max` bytes are allocated.
    pub fn str(&mut self, max: usize) -> Result<String> {
        let len = self.varint()?;
        if len > max as u64 {
            return Err(CodecError::OverLimit { what: "string length", limit: max as u64 });
        }
        let bytes = self.take(len as usize)?;
        let s = core::str::from_utf8(bytes).map_err(|_| CodecError::InvalidUtf8)?;
        if bytes.contains(&0) {
            return Err(CodecError::InteriorNul);
        }
        Ok(s.to_owned())
    }

    /// A vector count: ≤ `max`, and small enough that `count` items of at least `min_item_size` bytes fit in
    /// the bytes left. Call it before reserving anything.
    pub fn count(&mut self, max: usize, min_item_size: usize) -> Result<usize> {
        let count = self.varint()?;
        if count > max as u64 {
            return Err(CodecError::OverLimit { what: "vector count", limit: max as u64 });
        }
        let count = count as usize;
        match count.checked_mul(min_item_size) {
            Some(need) if need <= self.remaining() => Ok(count),
            _ => Err(CodecError::CountExceedsInput),
        }
    }
}

/// A value with a wire form. `lim` is the maximum of the outermost `str`/`Vec` (ignored by everything else);
/// the macros below pass the limit written in each field's declaration.
pub trait Wire: Sized {
    /// Fewest bytes any encoding of this type takes (used to bound declared counts).
    const MIN_SIZE: usize;

    fn put(&self, w: &mut Writer, lim: usize) -> Result<()>;

    fn get(r: &mut Reader<'_>, lim: usize) -> Result<Self>;

    /// The whole encoding of a message, header or record (the types that need no limit).
    fn encode(&self) -> Result<Vec<u8>> {
        let mut w = Writer::new();
        self.put(&mut w, 0)?;
        Ok(w.into_bytes())
    }

    /// Decodes `bytes` completely: trailing bytes are an error.
    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader::new(bytes);
        let v = Self::get(&mut r, 0)?;
        r.finish()?;
        Ok(v)
    }

    /// Decodes one value from the start of `bytes` and returns how many bytes it took
    /// ([`CodecError::Truncated`] means more bytes are needed).
    fn decode_prefix(bytes: &[u8]) -> Result<(Self, usize)> {
        let mut r = Reader::new(bytes);
        let v = Self::get(&mut r, 0)?;
        Ok((v, r.position()))
    }
}

macro_rules! wire_int {
    ($ty:ty, $put:ident, $get:ident) => {
        impl Wire for $ty {
            const MIN_SIZE: usize = core::mem::size_of::<$ty>();
            fn put(&self, w: &mut Writer, _lim: usize) -> Result<()> {
                w.$put(*self);
                Ok(())
            }
            fn get(r: &mut Reader<'_>, _lim: usize) -> Result<Self> {
                r.$get()
            }
        }
    };
}

wire_int!(u8, put_u8, u8);
wire_int!(u16, put_u16, u16);
wire_int!(u32, put_u32, u32);
wire_int!(u64, put_u64, u64);
wire_int!(i16, put_i16, i16);
wire_int!(i32, put_i32, i32);
wire_int!(bool, put_bool, bool);

impl<const N: usize> Wire for [u8; N] {
    const MIN_SIZE: usize = N;
    fn put(&self, w: &mut Writer, _lim: usize) -> Result<()> {
        w.put_bytes(self);
        Ok(())
    }
    fn get(r: &mut Reader<'_>, _lim: usize) -> Result<Self> {
        r.array()
    }
}

impl Wire for String {
    const MIN_SIZE: usize = 1;
    fn put(&self, w: &mut Writer, lim: usize) -> Result<()> {
        w.put_str(self, lim)
    }
    fn get(r: &mut Reader<'_>, lim: usize) -> Result<Self> {
        r.str(lim)
    }
}

impl<T: Wire> Wire for Option<T> {
    const MIN_SIZE: usize = 1;
    fn put(&self, w: &mut Writer, lim: usize) -> Result<()> {
        match self {
            None => w.put_u8(0),
            Some(v) => {
                w.put_u8(1);
                v.put(w, lim)?;
            }
        }
        Ok(())
    }
    fn get(r: &mut Reader<'_>, lim: usize) -> Result<Self> {
        match r.u8()? {
            0 => Ok(None),
            1 => Ok(Some(T::get(r, lim)?)),
            tag => Err(CodecError::UnknownTag { what: "option tag", tag: u64::from(tag) }),
        }
    }
}

impl<T: Wire> Wire for Vec<T> {
    const MIN_SIZE: usize = 1;
    fn put(&self, w: &mut Writer, lim: usize) -> Result<()> {
        if self.len() > lim {
            return Err(CodecError::OverLimit { what: "vector count", limit: lim as u64 });
        }
        w.put_varint(self.len() as u64)?;
        for item in self {
            item.put(w, 0)?;
        }
        Ok(())
    }
    fn get(r: &mut Reader<'_>, lim: usize) -> Result<Self> {
        let count = r.count(lim, T::MIN_SIZE)?;
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            out.push(T::get(r, 0)?);
        }
        Ok(out)
    }
}

#[doc(hidden)]
pub const fn sum_sizes(parts: &[usize]) -> usize {
    let mut total = 0;
    let mut i = 0;
    while i < parts.len() {
        total += parts[i];
        i += 1;
    }
    total
}

macro_rules! wire_lim {
    () => {
        0usize
    };
    ($e:expr) => {
        $e
    };
}

/// Stands in for a never-logged value (CONTRACTS.md C2.3) in `Debug` output.
pub struct Redacted;

impl core::fmt::Debug for Redacted {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Stands in for a never-logged byte or text body in `Debug` output: only its size shows (CONTRACTS.md C2.3).
/// Shared by every crate that holds payloads, so a `{:?}` can never print them.
pub struct RedactedLen(pub usize);

impl core::fmt::Debug for RedactedLen {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "<redacted {} bytes>", self.0)
    }
}

macro_rules! wire_debug_field {
    ($v:expr) => {
        &$v as &dyn core::fmt::Debug
    };
    ($v:expr, redact) => {
        &$crate::codec::Redacted as &dyn core::fmt::Debug
    };
}

/// A struct whose fields are written in order with no padding. A field may carry a limit in brackets
/// (`name: String [64]`, `items: Vec<Item> [16]`) and `=> redact` to hide its value from `Debug`
/// (tokens, nonces, chat text: CONTRACTS.md C2.3).
macro_rules! wire_struct {
    (
        $(#[$m:meta])*
        $name:ident {
            $( $(#[$fm:meta])* $f:ident : $ty:ty $([$lim:expr])? $(=> $red:ident)? ),* $(,)?
        }
    ) => {
        $(#[$m])*
        #[derive(Clone, PartialEq, Eq)]
        pub struct $name {
            $( $(#[$fm])* pub $f: $ty, )*
        }

        impl core::fmt::Debug for $name {
            #[allow(unused_variables)]
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.debug_struct(stringify!($name))
                    $( .field(stringify!($f), wire_debug_field!(self.$f $(, $red)?)) )*
                    .finish()
            }
        }

        impl $crate::codec::Wire for $name {
            const MIN_SIZE: usize =
                $crate::codec::sum_sizes(&[$(<$ty as $crate::codec::Wire>::MIN_SIZE),*]);

            #[allow(unused_variables)]
            fn put(&self, w: &mut $crate::codec::Writer, _lim: usize) -> $crate::codec::Result<()> {
                $( <$ty as $crate::codec::Wire>::put(&self.$f, w, wire_lim!($($lim)?))?; )*
                Ok(())
            }

            #[allow(unused_variables)]
            fn get(r: &mut $crate::codec::Reader<'_>, _lim: usize) -> $crate::codec::Result<Self> {
                Ok(Self {
                    $( $f: <$ty as $crate::codec::Wire>::get(r, wire_lim!($($lim)?))?, )*
                })
            }
        }
    };
}

/// A field-less enum with explicit tags; an unknown tag is a fatal error.
macro_rules! wire_enum {
    (
        $(#[$m:meta])*
        $name:ident : $repr:ident {
            $( $(#[$vm:meta])* $var:ident = $val:literal ),+ $(,)?
        }
    ) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name {
            $( $(#[$vm])* $var = $val, )+
        }

        impl $name {
            /// Every variant, in declaration order.
            pub const ALL: &'static [Self] = &[$(Self::$var),+];

            pub const fn tag(self) -> $repr {
                self as $repr
            }

            pub const fn from_tag(tag: $repr) -> Option<Self> {
                match tag {
                    $( $val => Some(Self::$var), )+
                    _ => None,
                }
            }
        }

        impl $crate::codec::Wire for $name {
            const MIN_SIZE: usize = core::mem::size_of::<$repr>();

            fn put(&self, w: &mut $crate::codec::Writer, _lim: usize) -> $crate::codec::Result<()> {
                <$repr as $crate::codec::Wire>::put(&self.tag(), w, 0)
            }

            fn get(r: &mut $crate::codec::Reader<'_>, _lim: usize) -> $crate::codec::Result<Self> {
                let tag = <$repr as $crate::codec::Wire>::get(r, 0)?;
                Self::from_tag(tag).ok_or($crate::codec::CodecError::UnknownTag {
                    what: stringify!($name),
                    tag: u64::from(tag),
                })
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varint_bytes(v: u64) -> Vec<u8> {
        let mut w = Writer::new();
        w.put_varint(v).unwrap();
        w.into_bytes()
    }

    #[test]
    fn an_appending_writer_keeps_the_bytes_and_the_capacity_it_was_given() {
        let mut buf = Vec::with_capacity(64);
        buf.extend_from_slice(&[9, 9]);
        let at = buf.as_ptr();
        let mut w = Writer::appending(buf);
        w.put_u16(0x0201);
        let out = w.into_bytes();
        assert_eq!(out, [9, 9, 1, 2]);
        assert_eq!((out.as_ptr(), out.capacity()), (at, 64), "the same buffer came back");
    }

    #[test]
    fn varint_matches_the_rfc_9000_examples() {
        assert_eq!(varint_bytes(37), [0x25]);
        assert_eq!(varint_bytes(15293), [0x7B, 0xBD]);
        assert_eq!(varint_bytes(494_878_333), [0x9D, 0x7F, 0x3E, 0x7D]);
        assert_eq!(varint_bytes(151_288_809_941_952_652), [0xC2, 0x19, 0x7C, 0x5E, 0xFF, 0x14, 0xE8, 0x8C]);
        assert_eq!(varint_bytes(MAX_VARINT), [0xFF; 8]);
    }

    #[test]
    fn varint_boundaries_use_the_shortest_form() {
        for (v, len) in [(63, 1), (64, 2), (16383, 2), (16384, 4), ((1 << 30) - 1, 4), (1 << 30, 8)] {
            let b = varint_bytes(v);
            assert_eq!(b.len(), len, "value {v}");
            assert_eq!(Reader::new(&b).varint().unwrap(), v);
        }
        assert_eq!(Writer::new().put_varint(MAX_VARINT + 1), Err(CodecError::VarintOutOfRange));
    }

    #[test]
    fn non_minimal_varints_are_rejected() {
        for bytes in [
            &[0x40, 0x25][..],
            &[0x80, 0x00, 0x00, 0x25],
            &[0x80, 0x00, 0x3F, 0xFF],
            &[0xC0, 0, 0, 0, 0, 0, 0, 0x25],
            &[0xC0, 0, 0, 0, 0x3F, 0xFF, 0xFF, 0xFF],
        ] {
            assert_eq!(Reader::new(bytes).varint(), Err(CodecError::NonMinimalVarint), "{bytes:x?}");
        }
    }

    #[test]
    fn truncated_varints_are_rejected_at_every_length() {
        let full = varint_bytes(1 << 40);
        for cut in 0..full.len() {
            assert_eq!(Reader::new(&full[..cut]).varint(), Err(CodecError::Truncated));
        }
    }

    #[test]
    fn bool_accepts_only_zero_and_one() {
        assert_eq!(Reader::new(&[0]).bool(), Ok(false));
        assert_eq!(Reader::new(&[1]).bool(), Ok(true));
        for b in 2..=255u8 {
            assert_eq!(Reader::new(&[b]).bool(), Err(CodecError::InvalidBool));
        }
    }

    #[test]
    fn strings_check_limit_utf8_and_nul() {
        let mut w = Writer::new();
        w.put_str("olá", 8).unwrap();
        let ok = w.into_bytes();
        assert_eq!(Reader::new(&ok).str(8).unwrap(), "olá");
        assert!(matches!(Reader::new(&ok).str(3), Err(CodecError::OverLimit { .. })));
        assert_eq!(Reader::new(&[2, 0xC3, 0x28]).str(8), Err(CodecError::InvalidUtf8));
        assert_eq!(Reader::new(&[2, b'a', 0]).str(8), Err(CodecError::InteriorNul));
        assert_eq!(Writer::new().put_str("a\0", 8), Err(CodecError::InteriorNul));
        assert!(Writer::new().put_str("abcd", 3).is_err());
    }

    #[test]
    fn a_huge_declared_length_is_rejected_before_anything_is_reserved() {
        // 2^62 - 1 bytes declared, none present: a decoder that reserved would abort the test process.
        let mut huge = vec![0xFF; 8];
        assert!(matches!(Reader::new(&huge).str(usize::MAX), Err(CodecError::Truncated)));
        assert!(matches!(Reader::new(&huge).str(64), Err(CodecError::OverLimit { .. })));
        assert_eq!(<Vec<u64> as Wire>::get(&mut Reader::new(&huge), usize::MAX), Err(CodecError::CountExceedsInput));
        huge.extend_from_slice(&[0; 64]);
        assert_eq!(<Vec<u64> as Wire>::get(&mut Reader::new(&huge), usize::MAX), Err(CodecError::CountExceedsInput));
    }

    #[test]
    fn count_is_bounded_by_the_bytes_left() {
        let mut r = Reader::new(&[3, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(r.count(16, 2), Ok(3));
        let mut r = Reader::new(&[5, 0, 0, 0]);
        assert_eq!(r.count(16, 2), Err(CodecError::CountExceedsInput));
    }

    #[test]
    fn option_tag_must_be_zero_or_one() {
        assert_eq!(<Option<u8> as Wire>::decode(&[0]), Ok(None));
        assert_eq!(<Option<u8> as Wire>::decode(&[1, 9]), Ok(Some(9)));
        assert!(matches!(<Option<u8> as Wire>::decode(&[2, 9]), Err(CodecError::UnknownTag { .. })));
    }

    #[test]
    fn fixed_ints_are_little_endian() {
        let mut w = Writer::new();
        w.put_u16(0x0102);
        w.put_u32(0x0304_0506);
        w.put_i16(-2);
        assert_eq!(w.into_bytes(), [0x02, 0x01, 0x06, 0x05, 0x04, 0x03, 0xFE, 0xFF]);
    }
}
