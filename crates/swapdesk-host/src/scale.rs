//! F-VIEW-06: the size and frame rate the host encodes at, decided from what the viewer reported in
//! `VideoSettings` (`view_width`, `view_height`, `max_fps`). Pure functions: no capture, no encoder.
//!
//! A view size of 0 in either axis means "native": the viewer is in 1:1 mode or does not report its area.

use swapdesk_proto::Preset;

/// The most frames per second Movimento fluido asks for (F-VIEW-06).
pub const MAX_MOTION_FPS: u16 = 120;
/// A new view size is acted on at most this often (microseconds).
pub const RESIZE_DEBOUNCE_US: u64 = 500_000;

/// The size to encode a `display`-sized capture at, for a viewer whose video area is `view` (physical pixels).
/// Automatica and Movimento scale down to the area, keeping the aspect, even dimensions, never above the display;
/// Texto nitido and an unreported or larger area stay native.
pub fn target_size(display: (u32, u32), view: (u32, u32), preset: Preset) -> (u32, u32) {
    let (dw, dh) = display;
    let (vw, vh) = view;
    if preset == Preset::Text || dw == 0 || dh == 0 || vw == 0 || vh == 0 || (vw >= dw && vh >= dh) {
        return (dw, dh);
    }
    // The largest size with the display's aspect that fits in the area: compare vw/dw with vh/dh without division.
    let (w, h) = if u64::from(vw) * u64::from(dh) <= u64::from(vh) * u64::from(dw) {
        (u64::from(vw), (u64::from(vw) * u64::from(dh)).div_ceil(u64::from(dw)))
    } else {
        ((u64::from(vh) * u64::from(dw)).div_ceil(u64::from(dh)), u64::from(vh))
    };
    // Even sizes (4:2:0), at least 2, never above the display (which may itself be odd).
    let even = |v: u64, max: u32| u32::try_from(v & !1).unwrap_or(max).clamp(2, max.max(2) & !1);
    (even(w, dw), even(h, dh))
}

/// The frame rate for a session. `max_fps` is the viewer's (0 = not reported: 60); `refresh_mhz` the host display's
/// (0 = unknown: 60 Hz). Movimento runs at the lowest of the two displays' rates and 120; the others keep the
/// viewer's rate, 1 to 240.
pub fn target_fps(preset: Preset, max_fps: u16, refresh_mhz: u32) -> u16 {
    let viewer = if max_fps == 0 { 60 } else { max_fps };
    if preset != Preset::Motion {
        return viewer.clamp(1, 240);
    }
    let host = if refresh_mhz == 0 { 60 } else { u16::try_from((refresh_mhz + 500) / 1000).unwrap_or(u16::MAX) };
    viewer.min(host).clamp(1, MAX_MOTION_FPS)
}

/// Holds a changing target size back so the encoder is rebuilt at most once per [`RESIZE_DEBOUNCE_US`].
#[derive(Debug, Default)]
pub struct SizeDebounce {
    applied: Option<(u32, u32)>,
    since_us: u64,
}

impl SizeDebounce {
    /// The size to encode at now: `wanted` when it differs from the applied one and the debounce has passed (the
    /// first size is applied at once), else the applied one.
    pub fn apply(&mut self, wanted: (u32, u32), now_us: u64) -> (u32, u32) {
        match self.applied {
            Some(cur) if cur == wanted => cur,
            Some(cur) if now_us.saturating_sub(self.since_us) < RESIZE_DEBOUNCE_US => cur,
            _ => {
                self.applied = Some(wanted);
                self.since_us = now_us;
                wanted
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: (u32, u32) = (2560, 1440);

    #[test]
    fn scales_down_keeping_aspect_and_even() {
        assert_eq!(target_size(D, (1280, 720), Preset::Auto), (1280, 720));
        assert_eq!(target_size(D, (1000, 1000), Preset::Motion), (1000, 562));
        assert_eq!(target_size(D, (1001, 563), Preset::Auto), (1000, 562));
        assert_eq!(target_size((1920, 1080), (801, 900), Preset::Auto), (800, 450));
    }

    #[test]
    fn native_for_text_unreported_or_larger() {
        assert_eq!(target_size(D, (800, 600), Preset::Text), D);
        assert_eq!(target_size(D, (0, 0), Preset::Auto), D);
        assert_eq!(target_size(D, (800, 0), Preset::Motion), D);
        assert_eq!(target_size(D, (3840, 2160), Preset::Auto), D);
        assert_eq!(target_size(D, (2560, 1440), Preset::Auto), D);
        assert_eq!(target_size((0, 0), (800, 600), Preset::Auto), (0, 0));
    }

    #[test]
    fn never_above_display_and_never_degenerate() {
        assert_eq!(target_size(D, (4000, 700), Preset::Auto), (1244, 700));
        let (w, h) = target_size(D, (1, 1), Preset::Auto);
        assert!(w >= 2 && h >= 2 && w % 2 == 0 && h % 2 == 0);
        let (w, h) = target_size((1921, 1081), (1920, 1080), Preset::Auto);
        assert!(w <= 1921 && h <= 1081 && w % 2 == 0 && h % 2 == 0);
    }

    #[test]
    fn motion_fps_is_the_lowest_of_both_displays_up_to_120() {
        assert_eq!(target_fps(Preset::Motion, 144, 165_000), 120);
        assert_eq!(target_fps(Preset::Motion, 90, 144_000), 90);
        assert_eq!(target_fps(Preset::Motion, 240, 59_940), 60);
        assert_eq!(target_fps(Preset::Motion, 120, 119_880), 120);
        assert_eq!(target_fps(Preset::Motion, 0, 144_000), 60);
        assert_eq!(target_fps(Preset::Motion, 120, 0), 60);
    }

    #[test]
    fn other_presets_keep_the_viewers_rate() {
        assert_eq!(target_fps(Preset::Auto, 0, 144_000), 60);
        assert_eq!(target_fps(Preset::Auto, 144, 60_000), 144);
        assert_eq!(target_fps(Preset::Text, 1000, 60_000), 240);
    }

    #[test]
    fn debounce_applies_first_at_once_then_at_most_every_500_ms() {
        let mut d = SizeDebounce::default();
        assert_eq!(d.apply((1280, 720), 0), (1280, 720));
        assert_eq!(d.apply((1000, 562), 100_000), (1280, 720));
        assert_eq!(d.apply((1000, 562), 499_999), (1280, 720));
        assert_eq!(d.apply((1000, 562), 500_000), (1000, 562));
        assert_eq!(d.apply((1000, 562), 600_000), (1000, 562));
        assert_eq!(d.apply((800, 450), 700_000), (1000, 562));
    }
}
