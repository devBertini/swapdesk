//! Format conversion to the C3 audio unit (48 kHz, stereo, interleaved f32). Pure code, built on every host.
//!
//! WASAPI converts for us when the stream is opened with `AUTOCONVERTPCM` (the normal path); this converter is the
//! fallback for a loopback endpoint that refuses that, where the stream runs in the endpoint's own mix format.

/// Output rate of every audio path.
pub const OUT_RATE: u32 = 48_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    F32,
    I16,
    /// 24 bits packed in 3 bytes.
    I24,
    /// 32-bit integers, or 24 bits in a 32-bit container (the same scale).
    I32,
}

impl SampleFormat {
    pub fn bytes(self) -> usize {
        match self {
            Self::I16 => 2,
            Self::I24 => 3,
            Self::F32 | Self::I32 => 4,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputFormat {
    pub rate: u32,
    pub channels: u16,
    pub format: SampleFormat,
}

pub struct Converter {
    input: InputFormat,
    /// Input frames per output frame.
    step: f64,
    /// Position of the next output frame between `prev` (0.0) and the next input frame (1.0), in input frames.
    pos: f64,
    prev: [f32; 2],
    have_prev: bool,
}

impl Converter {
    pub fn new(input: InputFormat) -> Self {
        Self {
            input,
            step: f64::from(input.rate.max(1)) / f64::from(OUT_RATE),
            pos: 0.0,
            prev: [0.0; 2],
            have_prev: false,
        }
    }

    pub fn input(&self) -> InputFormat {
        self.input
    }

    /// Converts `frames` input frames in `bytes` (or silence, when `silent`: the buffer content is undefined then) and
    /// appends the 48 kHz stereo result to `out`.
    pub fn push(&mut self, bytes: &[u8], frames: usize, silent: bool, out: &mut Vec<f32>) {
        let ch = usize::from(self.input.channels.max(1));
        let width = self.input.format.bytes();
        let frames = frames.min(bytes.len() / (ch * width).max(1));
        for i in 0..frames {
            let frame = if silent {
                [0.0, 0.0]
            } else {
                let base = i * ch * width;
                self.downmix(&bytes[base..base + ch * width])
            };
            self.feed(frame, out);
        }
    }

    fn sample(&self, b: &[u8]) -> f32 {
        match self.input.format {
            SampleFormat::F32 => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            SampleFormat::I16 => f32::from(i16::from_le_bytes([b[0], b[1]])) / 32_768.0,
            SampleFormat::I24 => (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0,
            SampleFormat::I32 => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0,
        }
    }

    /// Channels to stereo: mono is duplicated, stereo kept, more than two is the usual surround fold-down
    /// (front L R, centre, LFE dropped, rear and side channels at -3 dB).
    fn downmix(&self, frame: &[u8]) -> [f32; 2] {
        let w = self.input.format.bytes();
        let ch = usize::from(self.input.channels);
        let s = |i: usize| self.sample(&frame[i * w..(i + 1) * w]);
        match ch {
            1 => {
                let m = s(0);
                [m, m]
            }
            2 => [s(0), s(1)],
            _ => {
                const K: f32 = core::f32::consts::FRAC_1_SQRT_2;
                let (mut l, mut r) = (s(0), s(1));
                let mut gain = 1.0;
                if ch >= 3 {
                    l += K * s(2);
                    r += K * s(2);
                    gain += K;
                }
                // Channel 3 is the LFE: left out. 4 and 5 are the rear pair, 6 and 7 the side pair.
                for (li, ri) in [(4, 5), (6, 7)] {
                    if ch > ri {
                        l += K * s(li);
                        r += K * s(ri);
                        gain += K;
                    }
                }
                [l / gain, r / gain]
            }
        }
    }

    fn feed(&mut self, next: [f32; 2], out: &mut Vec<f32>) {
        if (self.step - 1.0).abs() < f64::EPSILON {
            out.extend_from_slice(&next);
            return;
        }
        if !self.have_prev {
            self.have_prev = true;
            self.prev = next;
            self.pos = 0.0;
            out.extend_from_slice(&next);
            self.pos = self.step;
            return;
        }
        // `pos` is where the next output frame falls, measured from `prev`; `next` is at 1.0.
        while self.pos < 1.0 {
            let t = self.pos as f32;
            out.push(self.prev[0] + (next[0] - self.prev[0]) * t);
            out.push(self.prev[1] + (next[1] - self.prev[1]) * t);
            self.pos += self.step;
        }
        self.pos -= 1.0;
        self.prev = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(rate: u32, channels: u16, format: SampleFormat) -> InputFormat {
        InputFormat { rate, channels, format }
    }

    fn f32_bytes(v: &[f32]) -> Vec<u8> {
        v.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    #[test]
    fn f32_stereo_48k_passes_through_exactly() {
        let mut c = Converter::new(fmt(48_000, 2, SampleFormat::F32));
        let v = [0.1, -0.2, 0.3, -0.4];
        let mut out = vec![];
        c.push(&f32_bytes(&v), 2, false, &mut out);
        assert_eq!(out, v);
    }

    #[test]
    fn mono_i16_becomes_stereo_and_scales() {
        let mut c = Converter::new(fmt(48_000, 1, SampleFormat::I16));
        let bytes: Vec<u8> = [16_384i16, -32_768].iter().flat_map(|x| x.to_le_bytes()).collect();
        let mut out = vec![];
        c.push(&bytes, 2, false, &mut out);
        assert_eq!(out, vec![0.5, 0.5, -1.0, -1.0]);
    }

    #[test]
    fn i24_and_i32_scale_to_unit() {
        let mut c = Converter::new(fmt(48_000, 2, SampleFormat::I24));
        // 0x400000 = half scale, 0xC00000 = minus half.
        let mut out = vec![];
        c.push(&[0x00, 0x00, 0x40, 0x00, 0x00, 0xC0], 1, false, &mut out);
        assert_eq!(out, vec![0.5, -0.5]);
        let mut c = Converter::new(fmt(48_000, 2, SampleFormat::I32));
        let bytes: Vec<u8> = [i32::MIN / 2, i32::MAX / 2].iter().flat_map(|x| x.to_le_bytes()).collect();
        let mut out = vec![];
        c.push(&bytes, 1, false, &mut out);
        assert!((out[0] + 0.5).abs() < 1e-6 && (out[1] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn silent_flag_ignores_the_buffer() {
        let mut c = Converter::new(fmt(48_000, 2, SampleFormat::F32));
        let mut out = vec![];
        c.push(&f32_bytes(&[0.9; 8]), 4, true, &mut out);
        assert_eq!(out, vec![0.0; 8]);
    }

    #[test]
    fn surround_folds_down_without_clipping() {
        let mut c = Converter::new(fmt(48_000, 6, SampleFormat::F32));
        let mut out = vec![];
        c.push(&f32_bytes(&[1.0; 6]), 1, false, &mut out);
        assert!(out[0] <= 1.0 && out[0] > 0.7, "{out:?}");
        assert_eq!(out[0], out[1]);
        // Only the LFE playing: silence.
        let mut c = Converter::new(fmt(48_000, 6, SampleFormat::F32));
        let mut out = vec![];
        c.push(&f32_bytes(&[0.0, 0.0, 0.0, 1.0, 0.0, 0.0]), 1, false, &mut out);
        assert_eq!(out, vec![0.0, 0.0]);
    }

    #[test]
    fn resampling_44100_to_48000_keeps_the_ratio_and_the_tone() {
        let mut c = Converter::new(fmt(44_100, 2, SampleFormat::F32));
        let n = 44_100;
        let tone: Vec<f32> = (0..n)
            .flat_map(|i| {
                let v = (core::f32::consts::TAU * 1_000.0 * i as f32 / 44_100.0).sin() * 0.5;
                [v, v]
            })
            .collect();
        let mut out = vec![];
        // Pushed in uneven chunks: the state carries across calls.
        for chunk in tone.chunks(2 * 441) {
            c.push(&f32_bytes(chunk), chunk.len() / 2, false, &mut out);
        }
        let frames = out.len() / 2;
        assert!(frames.abs_diff(48_000) <= 2, "{frames}");
        // The output is a 1 kHz tone: compare with the ideal one.
        let err = (100..frames - 100)
            .map(|i| {
                let ideal = (core::f32::consts::TAU * 1_000.0 * i as f32 / 48_000.0).sin() * 0.5;
                (out[i * 2] - ideal).abs()
            })
            .fold(0f32, f32::max);
        assert!(err < 0.02, "max error {err}");
    }
}
