//! Local copies of the two declaration macros of `swapdesk-proto` (which does not export them), written on its
//! public `Wire`, `Reader` and `Writer`.

macro_rules! wire_lim {
    () => {
        0usize
    };
    ($e:expr) => {
        $e
    };
}

/// A struct whose fields are written in order; a field may carry a limit in brackets (`name: String [64]`).
macro_rules! wire_struct {
    (
        $(#[$m:meta])*
        $name:ident {
            $( $(#[$fm:meta])* $f:ident : $ty:ty $([$lim:expr])? ),* $(,)?
        }
    ) => {
        $(#[$m])*
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct $name {
            $( $(#[$fm])* pub $f: $ty, )*
        }

        impl swapdesk_proto::Wire for $name {
            const MIN_SIZE: usize = 0 $( + <$ty as swapdesk_proto::Wire>::MIN_SIZE )*;

            #[allow(unused_variables)]
            fn put(&self, w: &mut swapdesk_proto::Writer, _lim: usize) -> Result<(), swapdesk_proto::CodecError> {
                $( <$ty as swapdesk_proto::Wire>::put(&self.$f, w, wire_lim!($($lim)?))?; )*
                Ok(())
            }

            #[allow(unused_variables)]
            fn get(r: &mut swapdesk_proto::Reader<'_>, _lim: usize) -> Result<Self, swapdesk_proto::CodecError> {
                Ok(Self {
                    $( $f: <$ty as swapdesk_proto::Wire>::get(r, wire_lim!($($lim)?))?, )*
                })
            }
        }
    };
}

/// A field-less `u8` enum with explicit tags; an unknown tag is an error.
macro_rules! wire_enum {
    (
        $(#[$m:meta])*
        $name:ident {
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

            pub const fn tag(self) -> u8 {
                self as u8
            }

            pub const fn from_tag(tag: u8) -> Option<Self> {
                match tag {
                    $( $val => Some(Self::$var), )+
                    _ => None,
                }
            }
        }

        impl swapdesk_proto::Wire for $name {
            const MIN_SIZE: usize = 1;

            fn put(&self, w: &mut swapdesk_proto::Writer, _lim: usize) -> Result<(), swapdesk_proto::CodecError> {
                w.put_u8(self.tag());
                Ok(())
            }

            fn get(r: &mut swapdesk_proto::Reader<'_>, _lim: usize) -> Result<Self, swapdesk_proto::CodecError> {
                let tag = r.u8()?;
                Self::from_tag(tag).ok_or(swapdesk_proto::CodecError::UnknownTag {
                    what: stringify!($name),
                    tag: u64::from(tag),
                })
            }
        }
    };
}

/// The error of a tag no variant of `$what` has.
pub(crate) fn unknown_tag(what: &'static str, tag: u8) -> swapdesk_proto::CodecError {
    swapdesk_proto::CodecError::UnknownTag { what, tag: u64::from(tag) }
}
