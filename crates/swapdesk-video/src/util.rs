//! A two-way select without the tokio `macros` feature: the first future to finish wins, the other is dropped.

use std::future::{Future, poll_fn};
use std::pin::pin;
use std::sync::{Mutex, MutexGuard};
use std::task::Poll;

pub(crate) enum Either<A, B> {
    Left(A),
    Right(B),
}

/// Polls `a` first, then `b`.
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

/// A lock that survives a panicked holder: transport state stays usable.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
