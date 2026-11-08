//! Network impairment for tests and measurements (contract C7). `Link` is the pure model of what happens to a packet;
//! `ImpairedSocket` puts it around a real UDP socket as a `quinn::AsyncUdpSocket`.

mod link;
mod socket;

pub use link::{DropReason, Impairment, Link, Rng, Verdict};
pub use socket::{ImpairedSocket, LinkHandle, Stats};
