//! The local terminal messages (C2.4 "Terminal"): the limits, the outcome of an open and the bodies shared by the
//! user-helper proxy messages (0x70 to 0x76) and the UI messages (0x78, 0x79).
//!
//! Terminal bytes are as private as typed text: every message here that holds them prints only a length in `Debug`
//! (they travel as [`Bytes`](crate::Bytes)), and every length is checked before a byte is allocated.

use swapdesk_proto::ErrorCode;

/// Largest chunk of terminal bytes in one `PtyInput`, `PtyOutput`, `TermInput` or `TermOutput`. The producers cut
/// longer data into chunks, so a chunk never needs more than this and a declared length above it is refused.
pub const MAX_TERM_CHUNK: usize = 64 * 1024;
/// Longest `PtyOpen.session_user` (a Windows SID text has at most 184 characters).
pub const MAX_SESSION_USER: usize = 184;
/// Longest account name of an opened terminal (`TermOpened.account` of PROTOCOL.md 5.3).
pub const MAX_TERM_ACCOUNT: usize = 64;
/// Body limit of `PtyInput`, `PtyOutput`, `TermInput` and `TermOutput`: the id and one chunk.
pub(crate) const MAX_TERM_BODY: usize = 4 + MAX_TERM_CHUNK;
/// Body limit of `PtyOpen` and `PtyOpenReply` (ids, sizes, one short text each).
pub(crate) const MAX_TERM_OPEN_BODY: usize = 512;
/// Body limit of `PtyResize`, `PtyClose` and `PtyExited`.
pub(crate) const MAX_TERM_SMALL_BODY: usize = 64;

wire_enum! {
    /// How `TermOpen` ended, in the reply body of the op (the op's status stays ok for every outcome but a request
    /// the service cannot place: unknown session, bad size, no administrator connection).
    TermOutcome {
        Opened = 0,
        /// The session does not hold `TERMINAL` (or `ADMIN_TERMINAL`), or the shell was refused as the wrong account.
        NotPermitted = 1,
        /// Nobody is logged in at the host (F-TERM-01).
        NobodyPresent = 2,
        /// The host's user said no to the D13 question.
        Declined = 3,
        /// The D13 question went unanswered, or the shell did not start in time.
        TimedOut = 4,
        /// Four terminals are open on the session already.
        TooMany = 5,
        /// No user helper (or no administrator spawner) is attached on the host.
        Unavailable = 6,
        Failed = 7,
    }
}

impl TermOutcome {
    /// The outcome a refusal code of the host stands for.
    pub fn of_error(code: ErrorCode) -> Self {
        match code {
            ErrorCode::NoError => Self::Opened,
            ErrorCode::NotPermitted => Self::NotPermitted,
            ErrorCode::NobodyPresent => Self::NobodyPresent,
            ErrorCode::Declined => Self::Declined,
            ErrorCode::Timeout => Self::TimedOut,
            ErrorCode::Busy => Self::TooMany,
            ErrorCode::Unavailable => Self::Unavailable,
            _ => Self::Failed,
        }
    }
}
