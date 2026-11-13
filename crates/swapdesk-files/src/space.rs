//! The free-space rule of F-FILE-03: a receiver declines an offer whose bytes do not fit, before writing anything.

use swapdesk_proto::ErrorCode;

/// Bytes kept free beyond the file itself, for the partial-file record and the file system's own bookkeeping.
pub const RESERVE_BYTES: u64 = 1 << 20;

/// Decides whether a transfer fits. `free` is the free bytes of the receiving volume (`None`: unknown, the transfer
/// goes on and a full disk later is an `ErrorCode::NoSpace` from the write), `size` the offered size and `have` the
/// bytes of the partial file that already count (a resume), which the volume holds already.
pub fn check_space(free: Option<u64>, size: u64, have: u64) -> Result<(), ErrorCode> {
    let Some(free) = free else { return Ok(()) };
    let need = size.saturating_sub(have);
    if need == 0 {
        return Ok(());
    }
    if need.saturating_add(RESERVE_BYTES) > free { Err(ErrorCode::NoSpace) } else { Ok(()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_space_goes_on() {
        assert_eq!(check_space(None, u64::MAX, 0), Ok(()));
    }

    #[test]
    fn fits_with_the_reserve() {
        let size = 10 << 20;
        assert_eq!(check_space(Some(size + RESERVE_BYTES), size, 0), Ok(()));
        assert_eq!(check_space(Some(size + RESERVE_BYTES - 1), size, 0), Err(ErrorCode::NoSpace));
    }

    #[test]
    fn a_resume_needs_only_the_rest() {
        let size = 10 << 20;
        assert_eq!(check_space(Some(RESERVE_BYTES + (2 << 20)), size, 8 << 20), Ok(()));
        assert_eq!(check_space(Some(0), size, size), Ok(()));
    }

    #[test]
    fn an_empty_file_always_fits() {
        assert_eq!(check_space(Some(0), 0, 0), Ok(()));
    }

    #[test]
    fn a_full_volume_refuses() {
        assert_eq!(check_space(Some(0), 1, 0), Err(ErrorCode::NoSpace));
    }
}
