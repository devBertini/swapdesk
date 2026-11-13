//! Stage W1.15 measurement: NVENC encode time per frame (median, p95, max) at 2560x1600 and 60 fps for each preset and AQ
//! choice, on synthetic frames only, paced at 60 fps. Latency of one call = `EncodedFrame::encode_us` (register, map,
//! submit, encode and read back the bitstream of that frame).
//!
//! Run alone: `cargo run --release -p swapdesk-codec-win --example preset_latency`. The textures live on the NVIDIA
//! adapter; NVENC itself works for any frame on that adapter, so no display owned by the NVIDIA GPU is needed.

#[cfg(windows)]
mod run {
    use std::time::{Duration, Instant};

    use std::sync::Arc;
    use swapdesk_codec_win::{NvencEncoder, NvencOptions, NvencTuning, TextureUploader, VENDOR_NVIDIA, list_adapters};
    use swapdesk_fakes::{FakeClock, SyntheticScreen};
    use swapdesk_platform::{
        CaptureConfig, DisplayId, EncoderConfig, Frame, MonoMicros, Pixels, ScreenSource, VideoEncoder,
    };
    use swapdesk_proto::Preset;
    use windows::Win32::Media::timeBeginPeriod;

    const W: u32 = 2560;
    const H: u32 = 1600;
    const FPS: u64 = 60;
    const WARMUP: usize = 30;
    const FRAMES: usize = 330;
    const KBPS: u32 = 30_000;
    const BUDGET_US: u32 = 6_000;

    fn percentile(sorted: &[u32], p: f64) -> u32 {
        sorted[((sorted.len() - 1) as f64 * p).round() as usize]
    }

    fn render(n: usize) -> Vec<Frame> {
        let clock = FakeClock::new(MonoMicros(5_000_000_000));
        let mut screen = SyntheticScreen::new(W, H, 60).with_clock(Arc::new(clock.clone()));
        screen.open(DisplayId(0), CaptureConfig { max_fps: 60 }).expect("open synthetic screen");
        (0..n)
            .map(|_| {
                let f = screen.next_frame(Duration::from_secs(1)).expect("frame").expect("some frame");
                clock.advance(Duration::from_micros(16_667));
                f
            })
            .collect()
    }

    pub fn main() {
        // SAFETY: raises the timer resolution for this process, as the other measurements do.
        unsafe { timeBeginPeriod(1) };
        let nv = list_adapters().expect("adapters").into_iter().find(|a| a.vendor_id == VENDOR_NVIDIA && !a.software);
        let Some(nv) = nv else {
            println!("no NVIDIA adapter on this PC");
            return;
        };
        let cpu_frames = render(24);
        let t = |preset, spatial_aq, temporal_aq, label: &'static str| {
            (label, NvencTuning { preset, spatial_aq, temporal_aq })
        };
        let rows = [
            t(1, false, false, "P1 (old Motion)"),
            t(2, false, false, "P2 (old Auto)"),
            t(3, false, false, "P3 (old Text)"),
            t(3, false, true, "P3 + temporal AQ (new Motion)"),
            t(3, true, false, "P3 + spatial AQ"),
            t(4, false, false, "P4 (new Auto)"),
            t(4, true, false, "P4 + spatial AQ"),
            t(5, false, false, "P5"),
            t(5, true, false, "P5 + spatial AQ (new Text)"),
            t(5, false, true, "P5 + temporal AQ"),
        ];
        println!(
            "NVENC encode time per frame, {W}x{H} at {FPS} fps, {KBPS} kbps, {FRAMES} frames after {WARMUP} of warm-up, budget {BUDGET_US} us\n"
        );
        println!("| setting | median ms | p95 ms | max ms | mean KB/frame | applied | within 6 ms (p95) |");
        println!("|---|---|---|---|---|---|---|");
        for (label, tuning) in rows {
            let uploader = TextureUploader::new(nv.id).expect("uploader");
            let mut enc = NvencEncoder::new(NvencOptions {
                device: Some(uploader.device()),
                tuning: Some(tuning),
                ..Default::default()
            });
            let cfg = EncoderConfig {
                width: W,
                height: H,
                fps: FPS as u16,
                kbps: KBPS,
                preset: Preset::Auto,
                adapter: nv.id,
                codec: swapdesk_platform::Codec::H264,
            };
            enc.configure(&cfg).expect("configure");
            let input: Vec<Frame> = cpu_frames.iter().map(|f| uploader.upload(f).expect("upload")).collect();
            let (mut times, mut bytes) = (Vec::new(), 0usize);
            let start = Instant::now();
            for i in 0..FRAMES {
                let due = start + Duration::from_nanos(1_000_000_000 / FPS * i as u64);
                if let Some(wait) = due.checked_duration_since(Instant::now()) {
                    std::thread::sleep(wait);
                }
                let g = &input[i % input.len()];
                let Pixels::Gpu(s) = &g.pixels else { unreachable!() };
                let tex =
                    s.0.downcast_ref::<windows::Win32::Graphics::Direct3D11::ID3D11Texture2D>()
                        .expect("texture")
                        .clone();
                let f = Frame {
                    display: g.display,
                    captured_at: MonoMicros(7_000_000_000 + i as u64 * 16_667),
                    width: g.width,
                    height: g.height,
                    pixels: Pixels::Gpu(swapdesk_platform::GpuSurface(Box::new(tex))),
                    dirty: Vec::new(),
                    content_changed: true,
                };
                for e in enc.encode(&f, false).expect("encode") {
                    if i >= WARMUP {
                        times.push(e.encode_us);
                        bytes += e.annex_b.len();
                    }
                }
            }
            times.sort_unstable();
            let s = enc.summary().expect("summary");
            let (m, p, x) = (percentile(&times, 0.5), percentile(&times, 0.95), *times.last().expect("frames"));
            println!(
                "| {label} | {:.2} | {:.2} | {:.2} | {:.0} | P{} spatial={} temporal={} | {} |",
                f64::from(m) / 1000.0,
                f64::from(p) / 1000.0,
                f64::from(x) / 1000.0,
                bytes as f64 / times.len() as f64 / 1024.0,
                s.preset,
                s.spatial_aq,
                s.temporal_aq,
                if p <= BUDGET_US { "yes" } else { "NO" }
            );
        }
    }
}

fn main() {
    #[cfg(windows)]
    run::main();
}
