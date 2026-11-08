//! Zeroing of key material once it is no longer needed. The crate forbids `unsafe`, so the write cannot be
//! volatile; `black_box` on the slice after the write and a compiler fence make the optimizer treat the zeros as
//! observable, which is what stops it from dropping a store to memory that is about to be freed.

use std::hint::black_box;
use std::sync::atomic::{Ordering, compiler_fence};

/// Overwrites every byte with zero.
pub fn wipe(bytes: &mut [u8]) {
    bytes.fill(0);
    black_box(&mut *bytes);
    compiler_fence(Ordering::SeqCst);
}

/// A buffer of secret bytes wiped when dropped (a record encoded for the disk, a blob read from it).
pub struct Secret(pub Vec<u8>);

impl Drop for Secret {
    fn drop(&mut self) {
        wipe(&mut self.0);
    }
}

impl std::ops::Deref for Secret {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wipe_zeroes_every_byte_and_accepts_an_empty_slice() {
        let mut v = vec![0xA5u8; 85];
        wipe(&mut v);
        assert!(v.iter().all(|&b| b == 0));
        wipe(&mut []);
    }

    #[test]
    fn secret_derefs_to_its_bytes() {
        let s = Secret(vec![1, 2, 3]);
        assert_eq!(&*s, &[1, 2, 3]);
    }
}
