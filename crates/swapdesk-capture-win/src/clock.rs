//! `MonoMicros` from QueryPerformanceCounter, the clock contract C3 names for every process of one machine.

use crate::errors::ticks_to_micros;
use std::sync::OnceLock;
use swapdesk_platform::{Clock, MonoMicros};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

/// QPC ticks per second (fixed at boot).
pub fn qpc_frequency() -> u64 {
    static FREQ: OnceLock<u64> = OnceLock::new();
    *FREQ.get_or_init(|| {
        let mut f = 0i64;
        // SAFETY: valid out pointer; cannot fail on Windows XP and later.
        let _ = unsafe { QueryPerformanceFrequency(&mut f) };
        u64::try_from(f).unwrap_or(0).max(1)
    })
}

pub fn qpc_ticks() -> u64 {
    let mut t = 0i64;
    // SAFETY: valid out pointer; cannot fail on Windows XP and later.
    let _ = unsafe { QueryPerformanceCounter(&mut t) };
    u64::try_from(t).unwrap_or(0)
}

pub fn ticks_to_mono(ticks: u64) -> MonoMicros {
    MonoMicros(ticks_to_micros(ticks, qpc_frequency()))
}

/// The machine's QPC clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct QpcClock;

impl Clock for QpcClock {
    fn now(&self) -> MonoMicros {
        ticks_to_mono(qpc_ticks())
    }
}
