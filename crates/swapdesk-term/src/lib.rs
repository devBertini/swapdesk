//! Terminal and quick-action relay of Swapdesk (stage 20; flows F-TERM-01, F-TERM-02, F-ACT-01; D13). OS-free and
//! transport-free, no `unsafe`: it is generic over the C3 traits (`PtySpawner`, `ActionRunner`) and exchanges plain
//! bytes and channels, so the session handler in the service (network side) and the user helper (PTY side) wire it
//! to QUIC streams and to the local pipe without this crate knowing either.
//!
//! * [`TermHost`]: opens PTYs for `TermOpen`, relays PTY output as [`TermEvent`]s and viewer input to the PTY, resizes,
//!   closes, ends on revoked grants. At most [`MAX_TERMINALS`] per session.
//! * [`ActionRelay`]: runs a `QuickAction` through an `ActionRunner` and answers with the `QuickActionResult`.
//! * [`consent`]: the D13 decision and the step that asks the logged-in user before a terminal of theirs opens.
//! * [`wire`]: the 0x10 stream header and the `PlatformError` to `ErrorCode` mapping.

pub mod actions;
pub mod consent;
pub mod host;
pub mod wire;

pub use actions::{ActionContext, ActionDone, ActionRelay};
pub use consent::{ConsentAsker, ConsentFacts, ConsentNeed, consent_need, run_consent};
pub use host::{InputTx, MAX_TERMINALS, OpenContext, TermEvent, TermHost};
pub use wire::{TERM_HEADER_LEN, code_for, parse_term_header, term_header};

#[cfg(test)]
mod tests;
