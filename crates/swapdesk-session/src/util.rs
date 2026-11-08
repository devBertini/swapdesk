//! Small helpers: poison-proof locking and random tokens.

use std::future::{Future, poll_fn};
use std::pin::pin;
use std::sync::{Mutex, MutexGuard};
use std::task::Poll;

use ring::rand::{SecureRandom, SystemRandom};

/// A lock that survives a panicked holder: session state stays usable (the service isolates the panic to its session).
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 16 random bytes from the system generator (resume and swap tokens).
pub fn random_token() -> [u8; 16] {
    let mut token = [0u8; 16];
    // The system generator failing is not recoverable; an all-zero token would be guessable, so refuse to go on.
    SystemRandom::new().fill(&mut token).expect("the system random generator failed");
    token
}

/// A random non-zero session id.
pub(crate) fn random_session_id() -> u64 {
    loop {
        let mut b = [0u8; 8];
        SystemRandom::new().fill(&mut b).expect("the system random generator failed");
        let v = u64::from_le_bytes(b);
        if v != 0 {
            return v;
        }
    }
}

/// Compares two tokens without stopping at the first difference.
pub fn tokens_equal(a: &[u8; 16], b: &[u8; 16]) -> bool {
    let diff = a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

pub(crate) enum Either<A, B> {
    Left(A),
    Right(B),
}

/// A two-way select without the tokio `macros` feature: polls `a` first, then `b`; the first to finish wins and the
/// other is dropped (so both must be cancel-safe).
pub(crate) async fn select2<A: Future, B: Future>(a: A, b: B) -> Either<A::Output, B::Output> {
    let mut a = pin!(a);
    let mut b = pin!(b);
    poll_fn(move |cx| {
        if let Poll::Ready(v) = a.as_mut().poll(cx) {
            return Poll::Ready(Either::Left(v));
        }
        if let Poll::Ready(v) = b.as_mut().poll(cx) {
            return Poll::Ready(Either::Right(v));
        }
        Poll::Pending
    })
    .await
}
