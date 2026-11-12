//! Enums, the permission set and the error codes shared by the messages (PROTOCOL.md 5.3, 6.1, 8, 9.2).

use core::ops::{BitAnd, BitOr};

use crate::codec::{CodecError, Reader, Result, Wire, Writer};

wire_enum! {
    /// Operating system of a peer (9.2: 0 Windows, 1 MacOS).
    Os: u8 { Windows = 0, MacOs = 1 }
}

wire_enum! {
    /// Video codec (9.2: 1 = H.264 Annex B, 2 = HEVC Annex B, minor 2).
    Codec: u8 { H264 = 1, Hevc = 2 }
}

impl Codec {
    /// The protocol minor that introduced this codec (PROTOCOL.md 1): a peer of an older minor is never offered it,
    /// never sent a capability naming it and never sent a frame in it.
    pub const fn min_minor(self) -> u16 {
        match self {
            Self::Hevc => 2,
            Self::H264 => 0,
        }
    }
}

wire_enum! {
    /// Audio codec (9.2: 1 = Opus, 48 kHz stereo).
    AudioCodec: u8 { Opus48Stereo = 1 }
}

wire_enum! {
    /// What the viewer wants to do (`Hello.intent`).
    Intent: u8 { View = 0, Control = 1 }
}

wire_enum! {
    H264Profile: u8 { Main = 0, High = 1 }
}

wire_enum! {
    KbKind: u8 { Ansi = 0, Iso = 1, Jis = 2, Abnt2 = 3, Unknown = 4 }
}

wire_enum! {
    GrantMode: u8 { Prompted = 0, Unattended = 1, Resumed = 2, Swap = 3 }
}

wire_enum! {
    /// Which desktop of the host receives input (`HostState.desktop`). `AccountAway` (minor 1): the account that
    /// hosts is not the one at the console.
    Desktop: u8 { Default = 0, Secure = 1, Locked = 2, LoginWindow = 3, AccountAway = 4 }
}

impl Desktop {
    /// The protocol minor that introduced this value (PROTOCOL.md 1): it is sent only to a peer of that minor or
    /// newer.
    pub const fn min_minor(self) -> u16 {
        match self {
            Self::AccountAway => 1,
            Self::Default | Self::Secure | Self::Locked | Self::LoginWindow => 0,
        }
    }

    /// What a peer of `minor` is told in place of this value: itself when the peer knows it, otherwise the value
    /// of its own minor that reads the same to a person (an account that is away looks locked).
    pub const fn for_minor(self, minor: u16) -> Self {
        if self.min_minor() <= minor { self } else { Self::Locked }
    }
}

wire_enum! {
    /// What a consent prompt is about (`ConsentPending.feature`).
    Feature: u8 { Terminal = 0, Files = 1 }
}

wire_enum! {
    /// Encoder tuning chosen by the viewer (`VideoSettings.preset`).
    Preset: u8 { Auto = 0, Text = 1, Motion = 2 }
}

wire_enum! {
    Special: u8 { SecureAttention = 0 }
}

wire_enum! {
    ClipSkip: u8 { Concealed = 0, TooLarge = 1, Unsupported = 2 }
}

wire_enum! {
    /// Quick actions (`QuickAction.kind`).
    Action: u8 { Lock = 0, Sleep = 1, Restart = 2, TaskMonitor = 3 }
}

wire_enum! {
    /// Mouse buttons of the `Button` input record (7.3).
    MouseButton: u8 { Left = 1, Right = 2, Middle = 3, Back = 4, Forward = 5 }
}

wire_enum! {
    /// Error codes (section 8); also the QUIC application close codes.
    ErrorCode: u16 {
        NoError = 0,
        ProtocolViolation = 1,
        UnsupportedVersion = 2,
        UnknownDevice = 3,
        NotPermitted = 4,
        NobodyPresent = 5,
        Declined = 6,
        Timeout = 7,
        Busy = 8,
        PairingLocked = 9,
        CommitMismatch = 10,
        SasRejected = 11,
        TooLarge = 12,
        NotFound = 13,
        IoFailed = 14,
        Unavailable = 15,
        Swapped = 16,
        HostShutdown = 17,
        Forgotten = 18,
        Internal = 19,
        InvalidName = 20,
        AlreadyPaired = 21,
        /// Minor 1: the account that would answer is not at the console of its computer.
        AccountAway = 22,
        /// Minor 2: the receiving volume has less free space than the file needs (F-FILE-03).
        NoSpace = 23,
        /// Minor 2: a delete to the bin on a volume that has no bin; nothing was deleted.
        NoBin = 24,
        /// Minor 2: a new folder, rename or move onto a name that exists; nothing is replaced.
        Exists = 25,
        /// Minor 2: the volume of a path is gone or now names another volume.
        VolumeGone = 26,
    }
}

impl ErrorCode {
    /// The protocol minor that introduced this code (PROTOCOL.md 1): it is sent only to a peer of that minor or
    /// newer.
    pub const fn min_minor(self) -> u16 {
        match self {
            Self::AccountAway => 1,
            Self::NoSpace | Self::NoBin | Self::Exists | Self::VolumeGone => 2,
            _ => 0,
        }
    }

    /// What a peer of `minor` is told in place of this code: itself when the peer knows it, otherwise the code
    /// of its own minor that says the same (an absent account reads as nobody there to answer; no space and an
    /// existing name as a failed write; a missing bin as not permitted; a volume that went away as not found).
    pub const fn for_minor(self, minor: u16) -> Self {
        if self.min_minor() <= minor {
            return self;
        }
        match self {
            Self::NoSpace | Self::Exists => Self::IoFailed,
            Self::NoBin => Self::NotPermitted,
            Self::VolumeGone => Self::NotFound,
            _ => Self::NobodyPresent,
        }
    }
}

/// `Capabilities.features` bits (5.3).
pub mod feature_bits {
    pub const CLIPBOARD_IMAGES: u32 = 1 << 0;
    pub const FILE_RESUME: u32 = 1 << 1;
    pub const TERMINAL: u32 = 1 << 2;
    pub const ADMIN_TERMINAL: u32 = 1 << 3;
    pub const QUICK_ACTIONS: u32 = 1 << 4;
    pub const SAS: u32 = 1 << 5;
}

/// A set of permissions, a `u16` bit set (6.1). Bits 8 to 15 are zero and a set always holds what its members
/// imply, so every value of this type is valid on the wire; [`PermissionSet::from_bits`] is the way in for
/// bits that came from outside.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PermissionSet(u16);

impl PermissionSet {
    pub const BIT_VIEW: u16 = 1 << 0;
    pub const BIT_CONTROL: u16 = 1 << 1;
    pub const BIT_AUDIO: u16 = 1 << 2;
    pub const BIT_TERMINAL: u16 = 1 << 3;
    pub const BIT_ADMIN_TERMINAL: u16 = 1 << 4;
    pub const BIT_FILES: u16 = 1 << 5;
    pub const BIT_CLIPBOARD: u16 = 1 << 6;
    pub const BIT_UNATTENDED: u16 = 1 << 7;
    const MASK: u16 = 0x00FF;

    pub const EMPTY: Self = Self(0);
    pub const VIEW: Self = Self(Self::BIT_VIEW);
    /// CONTROL implies VIEW.
    pub const CONTROL: Self = Self(Self::BIT_CONTROL | Self::BIT_VIEW);
    /// AUDIO implies VIEW.
    pub const AUDIO: Self = Self(Self::BIT_AUDIO | Self::BIT_VIEW);
    pub const TERMINAL: Self = Self(Self::BIT_TERMINAL);
    /// ADMIN_TERMINAL implies TERMINAL.
    pub const ADMIN_TERMINAL: Self = Self(Self::BIT_ADMIN_TERMINAL | Self::BIT_TERMINAL);
    pub const FILES: Self = Self(Self::BIT_FILES);
    pub const CLIPBOARD: Self = Self(Self::BIT_CLIPBOARD);
    /// UNATTENDED implies VIEW.
    pub const UNATTENDED: Self = Self(Self::BIT_UNATTENDED | Self::BIT_VIEW);

    pub const fn bits(self) -> u16 {
        self.0
    }

    /// `Some` only when no reserved bit is set and every implication holds.
    pub const fn from_bits(bits: u16) -> Option<Self> {
        if bits & !Self::MASK == 0 && Self::closure(bits).0 == bits { Some(Self(bits)) } else { None }
    }

    /// The smallest valid set that holds the known bits of `bits` (reserved bits dropped, implications added).
    pub const fn closure(bits: u16) -> Self {
        let mut b = bits & Self::MASK;
        if b & (Self::BIT_CONTROL | Self::BIT_AUDIO | Self::BIT_UNATTENDED) != 0 {
            b |= Self::BIT_VIEW;
        }
        if b & Self::BIT_ADMIN_TERMINAL != 0 {
            b |= Self::BIT_TERMINAL;
        }
        Self(b)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// True when every bit of `other` is in `self`.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }
}

impl BitOr for PermissionSet {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

impl BitAnd for PermissionSet {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self {
        self.intersection(rhs)
    }
}

impl Wire for PermissionSet {
    const MIN_SIZE: usize = 2;

    fn put(&self, w: &mut Writer, _lim: usize) -> Result<()> {
        w.put_u16(self.0);
        Ok(())
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> Result<Self> {
        Self::from_bits(r.u16()?).ok_or(CodecError::InvalidValue("permission set"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_code_numbers_match_section_8() {
        assert_eq!(ErrorCode::NoError.tag(), 0);
        assert_eq!(ErrorCode::SasRejected.tag(), 11);
        assert_eq!(ErrorCode::Internal.tag(), 19);
        assert_eq!(ErrorCode::AlreadyPaired.tag(), 21);
        assert_eq!(ErrorCode::AccountAway.tag(), 22);
        assert_eq!(ErrorCode::VolumeGone.tag(), 26);
        assert_eq!(ErrorCode::ALL.len(), 27);
        assert_eq!(ErrorCode::from_tag(27), None);
    }

    #[test]
    fn every_new_value_names_minor_1() {
        for code in ErrorCode::ALL {
            let want = match code.tag() {
                22 => 1,
                23.. => 2,
                _ => 0,
            };
            assert_eq!(code.min_minor(), want, "{code:?}");
            // A peer of minor 0 never gets a code it would refuse; a peer that knows the code gets it as it is.
            assert_eq!(code.for_minor(0).min_minor(), 0, "{code:?}");
            assert_eq!(code.for_minor(2), *code);
            assert!(code.for_minor(1).min_minor() <= 1, "{code:?}");
            assert_eq!(code.for_minor(u16::MAX), *code);
        }
        assert_eq!(ErrorCode::AccountAway.for_minor(0), ErrorCode::NobodyPresent);
        assert_eq!(ErrorCode::NoSpace.for_minor(0), ErrorCode::IoFailed);
        assert_eq!(ErrorCode::NoBin.for_minor(0), ErrorCode::NotPermitted);
        assert_eq!(ErrorCode::Exists.for_minor(0), ErrorCode::IoFailed);
        assert_eq!(ErrorCode::VolumeGone.for_minor(0), ErrorCode::NotFound);
        for codec in Codec::ALL {
            assert_eq!(codec.min_minor(), if *codec == Codec::Hevc { 2 } else { 0 }, "{codec:?}");
        }
        for desktop in Desktop::ALL {
            let want = u16::from(*desktop == Desktop::AccountAway);
            assert_eq!(desktop.min_minor(), want, "{desktop:?}");
            assert_eq!(desktop.for_minor(0).min_minor(), 0, "{desktop:?}");
            assert_eq!(desktop.for_minor(1), *desktop);
        }
        assert_eq!(Desktop::AccountAway.for_minor(0), Desktop::Locked);
        assert_eq!((Desktop::AccountAway.tag(), Desktop::ALL.len()), (4, 5));
    }

    #[test]
    fn explicit_tags_of_9_2() {
        assert_eq!((Os::Windows.tag(), Os::MacOs.tag()), (0, 1));
        assert_eq!((Codec::H264.tag(), Codec::Hevc.tag()), (1, 2));
        assert_eq!(AudioCodec::Opus48Stereo.tag(), 1);
        assert_eq!(Codec::from_tag(0), None);
    }

    #[test]
    fn permission_sets_enforce_reserved_bits_and_implications() {
        assert!(PermissionSet::from_bits(0).is_some());
        assert!(PermissionSet::from_bits(PermissionSet::BIT_CONTROL).is_none());
        assert!(PermissionSet::from_bits(PermissionSet::BIT_ADMIN_TERMINAL).is_none());
        assert!(PermissionSet::from_bits(PermissionSet::BIT_UNATTENDED).is_none());
        assert!(PermissionSet::from_bits(0x0100).is_none());
        assert_eq!(PermissionSet::from_bits(PermissionSet::UNATTENDED.bits()), Some(PermissionSet::UNATTENDED));
        assert_eq!(PermissionSet::closure(0xFFFF).bits(), 0x00FF);
        assert!((PermissionSet::CONTROL | PermissionSet::AUDIO).contains(PermissionSet::VIEW));
        assert_eq!(PermissionSet::CONTROL & PermissionSet::AUDIO, PermissionSet::VIEW);
    }

    #[test]
    fn every_closed_combination_is_valid() {
        for bits in 0..=u16::MAX {
            let closed = PermissionSet::closure(bits);
            assert_eq!(PermissionSet::from_bits(closed.bits()), Some(closed));
            if PermissionSet::from_bits(bits).is_none() {
                assert!(PermissionSet::decode(&bits.to_le_bytes()).is_err());
            }
        }
    }
}
