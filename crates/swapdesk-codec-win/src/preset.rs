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

/// PCI vendor id of NVIDIA.
pub const VENDOR_NVIDIA: u32 = 0x10DE;

/// How a hardware MFT follows a new mean bitrate while streaming. `ICodecAPI::IsModifiable` does not say it reliably,
/// so what was measured on each MFT decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveRate {
    /// Believe `IsModifiable(AVEncCommonMeanBitRate)` (Quick Sync: modifiable and followed under peak-constrained VBR,
    /// fixed under CBR; the software MFT); the peak and buffer stay as configured (a new peak made Quick Sync emit a
    /// keyframe, a new buffer size broke its next output).
    AsReported,
    /// The NVIDIA H.264 MFT: `IsModifiable` says fixed in every mode, yet under CBR it follows a new mean live, and a
    /// mean above the configured peak too once the peak is raised with it (measured 2026-11-09, driver 621.01, 1080p:
    /// 4000 -> 8000 -> 16000 -> 30000 -> 6000 kbps asked gave 4050, 8154, 16774, 29877 and 5304 kbps with no keyframe;
    /// without the new peak each raise rebuilt it). Under low-delay, peak-constrained and unconstrained VBR it kept
    /// 30.6 Mbps after 2000 was asked, with or without a new peak and buffer. So it runs CBR in every preset (a still
    /// picture costs 76 bytes a frame under its CBR: no padding).
    CbrWithPeak,
}

/// The live-rate rule of a hardware MFT on an adapter of `vendor_id` (the MFT enumerated for that adapter's LUID).
pub fn live_rate(vendor_id: u32, hardware: bool) -> LiveRate {
    if hardware && vendor_id == VENDOR_NVIDIA { LiveRate::CbrWithPeak } else { LiveRate::AsReported }
}

/// The `AVEncCommonRateControlMode` values to try, in order, for a preset under a live-rate rule.
pub fn rate_modes_for(preset: Preset, rule: LiveRate) -> &'static [u32] {
    match rule {
        LiveRate::AsReported => preset_plan(preset).rate_modes,
        LiveRate::CbrWithPeak => &[RATE_CBR],
    }
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

/// H.264 `level_idc` for a picture size, frame rate and mean bitrate in kbps: the lowest level of Table A-1 whose
/// MaxFS, MaxMBPS and MaxBR all hold the stream. MaxBR counts in High profile units (`cpbBrVclFactor` 1250, so
/// MaxBR * 1.25 kbps). The bitrate matters: the NVIDIA H.264 MFT refuses an output type whose `MF_MT_AVG_BITRATE` is over
/// its level's maximum with MF_E_UNSUPPORTED_D3D_TYPE (measured 2026-11-09: 1920x1080 at 30 fps took 30 Mbps at level
/// 4.0 and refused 40 Mbps, which Texto nítido asks for at 20 Mbps), even with no level in the type.
pub fn level_idc(width: u32, height: u32, fps: u32, kbps: u32) -> u32 {
    let mbs = width.div_ceil(16) * height.div_ceil(16);
    let rate = u64::from(mbs) * u64::from(fps);
    // (level_idc, MaxFS, MaxMBPS, MaxBR in 1000 * cpbBrVclFactor bits/s)
    const LEVELS: [(u32, u32, u64, u64); 11] = [
        (30, 1620, 40_500, 10_000),
        (31, 3600, 108_000, 14_000),
        (32, 5120, 216_000, 20_000),
        (40, 8192, 245_760, 20_000),
        (41, 8192, 245_760, 50_000),
        (42, 8704, 522_240, 50_000),
        (50, 22_080, 589_824, 135_000),
        (51, 36_864, 983_040, 240_000),
        (52, 36_864, 2_073_600, 240_000),
        (60, 139_264, 4_177_920, 240_000),
        (62, 139_264, 16_711_680, 800_000),
    ];
    let high_kbps = |max_br: u64| max_br * 5 / 4;
    LEVELS
        .iter()
        .find(|&&(_, fs, mbps, br)| mbs <= fs && rate <= mbps && u64::from(kbps) <= high_kbps(br))
        .map_or(62, |&(l, _, _, _)| l)
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
        assert_eq!(level_idc(1920, 1080, 30, 8_000), 40);
        assert_eq!(level_idc(1920, 1080, 60, 8_000), 42);
        assert_eq!(level_idc(2560, 1440, 60, 8_000), 51);
        assert_eq!(level_idc(3840, 2160, 30, 8_000), 51);
        assert_eq!(level_idc(3840, 2160, 60, 8_000), 52);
        assert_eq!(level_idc(1280, 720, 30, 8_000), 31);
    }

    #[test]
    fn the_nvidia_mft_runs_cbr_in_every_preset_and_the_others_keep_their_plan() {
        for preset in [Preset::Auto, Preset::Text, Preset::Motion] {
            let nv = live_rate(VENDOR_NVIDIA, true);
            assert_eq!(nv, LiveRate::CbrWithPeak);
            assert_eq!(rate_modes_for(preset, nv), &[RATE_CBR], "{preset:?}");
            for (vendor, hardware) in [(0x8086, true), (VENDOR_NVIDIA, false), (0x1414, false)] {
                let rule = live_rate(vendor, hardware);
                assert_eq!(rule, LiveRate::AsReported, "{vendor:#x} {hardware}");
                assert_eq!(rate_modes_for(preset, rule), preset_plan(preset).rate_modes, "{preset:?}");
            }
        }
    }

    #[test]
    fn the_level_also_holds_the_bitrate() {
        // Texto nítido at 20 Mbps: 1080p at 30 fps and 40 Mbps is over level 4.0 (25 Mbps in High) and fits 4.1.
        let text = effective(Preset::Text, 20_000, 60);
        assert_eq!(level_idc(1920, 1080, text.output_fps, text.kbps), 41);
        assert_eq!(level_idc(1920, 1080, 30, 25_000), 40);
        assert_eq!(level_idc(1920, 1080, 30, 25_001), 41);
        assert_eq!(level_idc(1920, 1080, 60, 62_500), 42);
        assert_eq!(level_idc(1920, 1080, 60, 70_000), 50);
        assert_eq!(level_idc(2560, 1440, 30, 160_000), 50);
        assert_eq!(level_idc(1280, 720, 30, 200_000), 51);
        assert_eq!(level_idc(1280, 720, 30, 400_000), 62);
    }
}
