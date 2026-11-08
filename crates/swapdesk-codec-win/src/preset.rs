//! What each viewer preset (F-VIEW-03) asks of the encoder. Pure code.

use swapdesk_proto::Preset;

/// `AVEncCommonRateControlMode` values the encoder uses.
pub const RATE_CBR: u32 = 0;
pub const RATE_PEAK_CONSTRAINED_VBR: u32 = 1;
pub const RATE_LOW_DELAY_VBR: u32 = 4;
/// Auto and Text prefer low-delay VBR, then peak-constrained VBR (a still screen costs few bits; Quick Sync follows a
/// bitrate change live in this mode and not under CBR), then CBR. Neither Quick Sync nor the software MFT here takes
/// low-delay VBR.
pub const LOW_DELAY_ORDER: [u32; 3] = [RATE_LOW_DELAY_VBR, RATE_PEAK_CONSTRAINED_VBR, RATE_CBR];

/// The encoder settings behind one preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresetPlan {
    /// The most frames per second the preset encodes; more frames arriving are dropped by capture time.
    pub max_fps: u32,
    /// Bitrate multiplier in percent: Texto nítido spends twice the bits (4:2:0 is the only chroma format every
    /// MF encoder and decoder share, so sharpness comes from bits and a lower frame rate, windows.md 3c).
    pub kbps_percent: u32,
    /// `AVEncCommonRateControlMode` values in order of preference; the first one the MFT accepts is used.
    pub rate_modes: &'static [u32],
    /// `AVEncCommonQualityVsSpeed`, 0 (fastest) to 100 (best quality).
    pub quality_vs_speed: u32,
}

/// Frame-rate cap of Texto nítido.
pub const TEXT_MAX_FPS: u32 = 30;
/// Largest bitrate the encoder is ever asked for, in kbps.
pub const MAX_KBPS: u32 = 200_000;

pub fn preset_plan(preset: Preset) -> PresetPlan {
    match preset {
        Preset::Auto => {
            PresetPlan { max_fps: u32::MAX, kbps_percent: 100, rate_modes: &LOW_DELAY_ORDER, quality_vs_speed: 50 }
        }
        Preset::Text => {
            PresetPlan { max_fps: TEXT_MAX_FPS, kbps_percent: 200, rate_modes: &LOW_DELAY_ORDER, quality_vs_speed: 100 }
        }
        Preset::Motion => {
            PresetPlan { max_fps: u32::MAX, kbps_percent: 100, rate_modes: &[RATE_CBR], quality_vs_speed: 25 }
        }
    }
}

/// Peak bitrate and buffer size for a mean bitrate, both in bits: the ceiling is twice the mean in VBR and the mean in CBR,
/// and the buffer holds three frames at `fps` of the ceiling, so one IDR of a sharp screen fits without a long delay.
/// The same rule as the NVENC rate-control block (docs/reviews/2026-11-08-evaluation.md L2).
pub fn rate_limits(mean_bps: u32, cbr: bool, fps: u32) -> (u32, u32) {
    let max = if cbr { mean_bps } else { mean_bps.saturating_mul(2) };
    let buffer = (u64::from(mean_bps) * 3 / u64::from(fps.max(1))).min(u64::from(u32::MAX)) as u32;
    (max, buffer)
}

/// The numbers a request turns into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Effective {
    /// Frames per second arriving from capture.
    pub input_fps: u32,
    /// Frames per second the encoder encodes (the preset cap applied).
    pub output_fps: u32,
    /// Target bitrate after the preset multiplier, kbps.
    pub kbps: u32,
}

pub fn effective(preset: Preset, kbps: u32, fps: u16) -> Effective {
    let plan = preset_plan(preset);
    let input_fps = u32::from(fps).max(1);
    Effective {
        input_fps,
        output_fps: input_fps.min(plan.max_fps),
        kbps: (u64::from(kbps) * u64::from(plan.kbps_percent) / 100).min(u64::from(MAX_KBPS)) as u32,
    }
}

/// H.264 `level_idc` for a picture size and frame rate (Table A-1 macroblock rates and frame sizes).
pub fn level_idc(width: u32, height: u32, fps: u32) -> u32 {
    let mbs = width.div_ceil(16) * height.div_ceil(16);
    let rate = u64::from(mbs) * u64::from(fps);
    // (level_idc, MaxFS, MaxMBPS)
    const LEVELS: [(u32, u32, u64); 10] = [
        (30, 1620, 40_500),
        (31, 3600, 108_000),
        (32, 5120, 216_000),
        (40, 8192, 245_760),
        (42, 8704, 522_240),
        (50, 22_080, 589_824),
        (51, 36_864, 983_040),
        (52, 36_864, 2_073_600),
        (60, 139_264, 4_177_920),
        (62, 139_264, 16_711_680),
    ];
    LEVELS.iter().find(|&&(_, fs, mbps)| mbs <= fs && rate <= mbps).map_or(62, |&(l, _, _)| l)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_halves_the_frame_rate_and_doubles_the_bits() {
        let e = effective(Preset::Text, 8000, 60);
        assert_eq!((e.input_fps, e.output_fps, e.kbps), (60, 30, 16_000));
        let a = effective(Preset::Auto, 8000, 60);
        assert_eq!((a.input_fps, a.output_fps, a.kbps), (60, 60, 8000));
        let m = effective(Preset::Motion, 8000, 120);
        assert_eq!((m.output_fps, m.kbps), (120, 8000));
        assert_eq!(effective(Preset::Text, 150_000, 24).kbps, MAX_KBPS);
        assert_eq!(effective(Preset::Text, 8000, 24).output_fps, 24);
    }

    #[test]
    fn rate_limits_are_twice_the_mean_in_vbr_and_the_mean_in_cbr_with_a_three_frame_buffer() {
        assert_eq!(rate_limits(8_000_000, false, 60), (16_000_000, 400_000));
        assert_eq!(rate_limits(8_000_000, true, 60), (8_000_000, 400_000));
        assert_eq!(rate_limits(u32::MAX, false, 1), (u32::MAX, u32::MAX));
        assert_eq!(rate_limits(1_000_000, true, 0).1, 3_000_000);
    }

    #[test]
    fn levels_for_common_sizes() {
        assert_eq!(level_idc(1920, 1080, 30), 40);
        assert_eq!(level_idc(1920, 1080, 60), 42);
        assert_eq!(level_idc(2560, 1440, 60), 51);
        assert_eq!(level_idc(3840, 2160, 30), 51);
        assert_eq!(level_idc(3840, 2160, 60), 52);
        assert_eq!(level_idc(1280, 720, 30), 31);
    }
}
