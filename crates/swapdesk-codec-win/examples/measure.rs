//! Stage 13 measurements: encode and decode latency per frame (median and p95) and CPU time per second at 1920x1080 and
//! 60 fps, for every encoder MFT and decoder path of this PC, on synthetic frames only (never the real screen).
//!
//! Run: `cargo run --release -p swapdesk-codec-win --example measure > test-results/stage13-measure.md`
//! (release: the synthetic renderer and the colour conversion are compute-heavy; the frames are rendered before the clock).
//! Latency of one encode call = `EncodedFrame::encode_us`: from the call to the output of that frame (upload or texture
//! conversion BGRA to NV12 on the GPU, the MFT, the wait for its output). Latency of one decode call = from the access
//! unit to the picture (NV12 texture copy, or the BGRA readback for the CPU path).

#[cfg(windows)]
mod run {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use swapdesk_codec_win::{
        DecoderOptions, DecoderOutput, EncoderOptions, MfH264Decoder, MfH264Encoder, MftChoice, NvencEncoder,
        NvencOptions, TextureUploader, VENDOR_NVIDIA, list_adapters, probe_encoders,
    };
    use swapdesk_fakes::{FakeClock, SyntheticScreen};
    use swapdesk_platform::{
        AdapterId, CaptureConfig, DisplayId, EncodedFrame, EncoderConfig, Frame, MonoMicros, Pixels, ScreenSource,
        VideoEncoder,
    };
    use swapdesk_proto::{Codec, Preset, VideoHeader};
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::Media::timeBeginPeriod;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

    const W: u32 = 1920;
    const H: u32 = 1080;
    const FPS: u32 = 60;
    const WARMUP: usize = 60;
    const FRAMES: usize = 660;
    const KBPS: u32 = 20_000;

    fn cpu_seconds() -> f64 {
        let (mut c, mut e, mut k, mut u) =
            (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        // SAFETY: GetProcessTimes fills four FILETIMEs for the current process.
        unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) }.expect("GetProcessTimes");
        let ticks = |f: FILETIME| (u64::from(f.dwHighDateTime) << 32 | u64::from(f.dwLowDateTime)) as f64 * 100e-9;
        ticks(k) + ticks(u)
    }

    fn percentile(sorted: &[u32], p: f64) -> u32 {
        if sorted.is_empty() {
            return 0;
        }
        sorted[((sorted.len() - 1) as f64 * p).round() as usize]
    }

    pub struct Row {
        pub label: String,
        pub note: String,
        pub median_us: u32,
        pub p95_us: u32,
        pub max_us: u32,
        pub convert_median_us: u32,
        pub cpu_percent_of_core: f64,
        pub kbps: f64,
        pub frames: usize,
    }

    impl Row {
        fn print_decode(&self) {
            println!(
                "| {} | {} | {:.2} | {:.2} | {:.2} | {:.1} | {} |",
                self.label,
                self.note,
                f64::from(self.median_us) / 1000.0,
                f64::from(self.p95_us) / 1000.0,
                f64::from(self.max_us) / 1000.0,
                self.cpu_percent_of_core,
                self.frames
            );
        }

        fn print(&self) {
            println!(
                "| {} | {} | {:.2} | {:.2} | {:.2} | {:.2} | {:.1} | {:.0} | {} |",
                self.label,
                self.note,
                f64::from(self.median_us) / 1000.0,
                f64::from(self.p95_us) / 1000.0,
                f64::from(self.max_us) / 1000.0,
                f64::from(self.convert_median_us) / 1000.0,
                self.cpu_percent_of_core,
                self.kbps,
                self.frames
            );
        }
    }

    fn stats(label: &str, note: &str, mut us: Vec<u32>, mut conv: Vec<u32>, cpu: f64, secs: f64, bytes: usize) -> Row {
        let frames = us.len();
        us.sort_unstable();
        conv.sort_unstable();
        Row {
            label: label.to_string(),
            note: note.to_string(),
            median_us: percentile(&us, 0.5),
            p95_us: percentile(&us, 0.95),
            max_us: us.last().copied().unwrap_or(0),
            convert_median_us: percentile(&conv, 0.5),
            cpu_percent_of_core: cpu / secs * 100.0,
            kbps: bytes as f64 * 8.0 / secs / 1000.0,
            frames,
        }
    }

    fn render_frames() -> Vec<Frame> {
        render_sized(W, H, 60)
    }

    fn render_sized(w: u32, h: u32, n: usize) -> Vec<Frame> {
        let clock = FakeClock::new(MonoMicros(7_000_000_000));
        let mut screen = SyntheticScreen::new(w, h, FPS as u16).with_clock(Arc::new(clock.clone()));
        screen.open(DisplayId(0), CaptureConfig { max_fps: FPS as u16 }).expect("open");
        let mut frames = Vec::new();
        for _ in 0..n {
            frames.push(screen.next_frame(Duration::from_secs(1)).expect("frame").expect("some frame"));
            clock.advance(Duration::from_micros(16_667));
        }
        frames
    }

    fn pace(i: usize, start: Instant) {
        let due = start + Duration::from_nanos(1_000_000_000 / u64::from(FPS) * i as u64);
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        }
    }

    /// Encodes `FRAMES` frames paced at 60 fps. `gpu_input`: frames are textures made on the encoder's own device.
    fn encode_run(
        label: &str,
        clsid: &str,
        adapter: AdapterId,
        gpu_input: bool,
        cpu_frames: &[Frame],
    ) -> Result<(Row, Vec<EncodedFrame>), String> {
        let uploader = TextureUploader::new(adapter).map_err(|e| e.to_string())?;
        let mut enc = MfH264Encoder::new(EncoderOptions {
            choice: MftChoice::Clsid(clsid.to_string()),
            device: if gpu_input { Some(uploader.device()) } else { None },
            ..Default::default()
        });
        let cfg = EncoderConfig {
            width: W,
            height: H,
            fps: FPS as u16,
            kbps: KBPS,
            preset: Preset::Auto,
            adapter,
            codec: swapdesk_platform::Codec::H264,
        };
        enc.configure(&cfg).map_err(|e| e.to_string())?;
        let input: Vec<Frame> = if gpu_input {
            cpu_frames.iter().map(|f| uploader.upload(f).expect("upload")).collect()
        } else {
            Vec::new()
        };
        let mut encoded = Vec::with_capacity(FRAMES);
        let mut conv = Vec::new();
        let (mut cpu0, mut t0) = (0.0, Instant::now());
        let start = Instant::now();
        for i in 0..FRAMES {
            pace(i, start);
            if i == WARMUP {
                cpu0 = cpu_seconds();
                t0 = Instant::now();
            }
            let mut f = if gpu_input {
                let g = &input[i % input.len()];
                let Pixels::Gpu(s) = &g.pixels else { unreachable!() };
                let tex =
                    s.0.downcast_ref::<windows::Win32::Graphics::Direct3D11::ID3D11Texture2D>()
                        .expect("texture")
                        .clone();
                Frame {
                    display: g.display,
                    captured_at: g.captured_at,
                    width: g.width,
                    height: g.height,
                    pixels: Pixels::Gpu(swapdesk_platform::GpuSurface(Box::new(tex))),
                    dirty: Vec::new(),
                    content_changed: true,
                }
            } else {
                let g = &cpu_frames[i % cpu_frames.len()];
                let Pixels::Cpu { stride, data } = &g.pixels else { unreachable!() };
                Frame {
                    display: g.display,
                    captured_at: g.captured_at,
                    width: g.width,
                    height: g.height,
                    pixels: Pixels::Cpu { stride: *stride, data: data.clone() },
                    dirty: Vec::new(),
                    content_changed: true,
                }
            };
            f.captured_at = MonoMicros(7_000_000_000 + i as u64 * 16_667);
            let out = enc.encode(&f, false).map_err(|e| format!("frame {i}: {e}"))?;
            if i >= WARMUP {
                conv.push(enc.last_timing().convert_us);
            }
            encoded.extend(out);
        }
        let secs = t0.elapsed().as_secs_f64();
        let cpu = cpu_seconds() - cpu0;
        encoded.extend(enc.flush().map_err(|e| e.to_string())?);
        let timed: Vec<&EncodedFrame> = encoded.iter().skip(WARMUP).collect();
        let bytes: usize = timed.iter().map(|e| e.annex_b.len()).sum();
        let us: Vec<u32> = timed.iter().map(|e| e.encode_us).collect();
        let note = if gpu_input { "GPU texture in" } else { "CPU frame in" };
        let row = stats(label, note, us, conv, cpu, secs, bytes);
        Ok((row, encoded))
    }

    fn decode_run(
        label: &str,
        adapter: AdapterId,
        dxva: bool,
        output: DecoderOutput,
        stream: &[EncodedFrame],
    ) -> Result<Row, String> {
        let mut dec = MfH264Decoder::new(DecoderOptions { output, adapter, dxva, ..Default::default() });
        let mut us = Vec::new();
        let (mut cpu0, mut t0) = (0.0, Instant::now());
        let start = Instant::now();
        let mut pictures = 0usize;
        for (i, e) in stream.iter().enumerate() {
            pace(i, start);
            if i == WARMUP {
                cpu0 = cpu_seconds();
                t0 = Instant::now();
            }
            let header = VideoHeader {
                keyframe: e.keyframe,
                config: e.config_change,
                discontinuity: false,
                display_id: 0,
                frame_id: i as u32,
                capture_us: e.captured_at.0,
                encode_us: e.encode_us,
                width: e.width,
                height: e.height,
                codec: Codec::H264,
                payload_len: e.annex_b.len() as u32,
            };
            let got = dec.decode_settled(&header, &e.annex_b).map_err(|e| format!("frame {i}: {e}"))?;
            if got.is_some() {
                pictures += 1;
            }
            if i >= WARMUP {
                us.push(dec.last_decode_us());
            }
        }
        let secs = t0.elapsed().as_secs_f64();
        let cpu = cpu_seconds() - cpu0;
        let (name, accelerated) = dec.mft_name().map(|(n, d)| (n.to_string(), d)).unwrap_or_default();
        let note = format!(
            "{name}, {}, {}",
            if accelerated { "DXVA" } else { "software" },
            match output {
                DecoderOutput::Gpu => "NV12 texture out",
                DecoderOutput::CpuBgra => "BGRA readback",
            }
        );
        if pictures + 2 < stream.len() {
            return Err(format!("only {pictures} pictures from {} access units", stream.len()));
        }
        Ok(stats(label, &note, us, Vec::new(), cpu, secs, 0))
    }

    /// Stage 35: one paced run of `FRAMES` texture frames at `size` through NVENC (`clsid` is `None`) or the Media
    /// Foundation encoder with that CLSID (Quick Sync) on `adapter`, for the latency and CPU comparison.
    fn compare_run(
        label: &str,
        adapter: AdapterId,
        size: (u32, u32),
        clsid: Option<&str>,
        cpu_frames: &[Frame],
    ) -> Result<Row, String> {
        let uploader = TextureUploader::new(adapter).map_err(|e| e.to_string())?;
        let cfg = EncoderConfig {
            width: size.0,
            height: size.1,
            fps: FPS as u16,
            kbps: KBPS,
            preset: Preset::Auto,
            adapter,
            codec: swapdesk_platform::Codec::H264,
        };
        let mut nv;
        let mut mf;
        let enc: &mut dyn VideoEncoder = match clsid {
            None => {
                nv = NvencEncoder::new(NvencOptions { device: Some(uploader.device()), ..Default::default() });
                &mut nv
            }
            Some(c) => {
                mf = MfH264Encoder::new(EncoderOptions {
                    choice: MftChoice::Clsid(c.to_string()),
                    device: Some(uploader.device()),
                    ..Default::default()
                });
                &mut mf
            }
        };
        enc.configure(&cfg).map_err(|e| e.to_string())?;
        let input: Vec<Frame> = cpu_frames.iter().map(|f| uploader.upload(f).expect("upload")).collect();
        let mut encoded: Vec<EncodedFrame> = Vec::with_capacity(FRAMES);
        let (mut cpu0, mut t0) = (0.0, Instant::now());
        let start = Instant::now();
        for i in 0..FRAMES {
            pace(i, start);
            if i == WARMUP {
                cpu0 = cpu_seconds();
                t0 = Instant::now();
            }
            let g = &input[i % input.len()];
            let Pixels::Gpu(s) = &g.pixels else { unreachable!() };
            let tex =
                s.0.downcast_ref::<windows::Win32::Graphics::Direct3D11::ID3D11Texture2D>().expect("texture").clone();
            let f = Frame {
                display: g.display,
                captured_at: MonoMicros(7_000_000_000 + i as u64 * 16_667),
                width: g.width,
                height: g.height,
                pixels: Pixels::Gpu(swapdesk_platform::GpuSurface(Box::new(tex))),
                dirty: Vec::new(),
                content_changed: true,
            };
            encoded.extend(enc.encode(&f, false).map_err(|e| format!("frame {i}: {e}"))?);
        }
        let secs = t0.elapsed().as_secs_f64();
        let cpu = cpu_seconds() - cpu0;
        encoded.extend(enc.flush().map_err(|e| e.to_string())?);
        let timed: Vec<&EncodedFrame> = encoded.iter().skip(WARMUP).collect();
        let bytes: usize = timed.iter().map(|e| e.annex_b.len()).sum();
        let us: Vec<u32> = timed.iter().map(|e| e.encode_us).collect();
        Ok(stats(label, &format!("{}x{} GPU texture in", size.0, size.1), us, Vec::new(), cpu, secs, bytes))
    }

    /// The NVENC against Quick Sync table of stage 35 (D21), at 1080p60 and 1440p60.
    fn stage35() {
        println!(
            "# Stage 35 measurements: NVENC (D21) against Quick Sync, {FPS} fps paced, {KBPS} kbps, Automatica, {FRAMES} frames (first {WARMUP} not counted)"
        );
        println!();
        println!(
            "Synthetic frames uploaded to a texture on the encoder's own adapter; encode latency is `EncodedFrame::encode_us` (call to output of the same frame, including colour conversion where the path has one). CPU is process CPU time (user + kernel, all threads) as a percentage of one core while the paced run goes. Release profile."
        );
        println!();
        let adapters = list_adapters().expect("adapters");
        for a in &adapters {
            println!("- {} (LUID {:#x}, vendor {:#06x}, software {})", a.name, a.id.0, a.vendor_id, a.software);
        }
        println!();
        println!(
            "| path | input | median ms | p95 ms | max ms | of which convert ms (median) | CPU % of a core | output kbps | frames |"
        );
        println!("|---|---|---|---|---|---|---|---|---|");
        let probes = probe_encoders().expect("probe");
        for size in [(1920u32, 1080u32), (2560, 1440)] {
            let cpu_frames = render_sized(size.0, size.1, 20);
            for a in adapters.iter().filter(|a| !a.software) {
                if a.vendor_id == VENDOR_NVIDIA {
                    let label = format!("NVENC direct on {}", a.name);
                    match compare_run(&label, a.id, size, None, &cpu_frames) {
                        Ok(row) => row.print(),
                        Err(e) => println!("| {label} | - | failed: {} | | | | | | |", e.replace('|', "/")),
                    }
                }
                for p in probes.iter().filter(|p| p.adapter.id == a.id && a.vendor_id == 0x8086) {
                    let label = format!("{} on {}", p.name, a.name);
                    match &p.mft {
                        Ok(m) => match compare_run(&label, a.id, size, Some(&m.clsid), &cpu_frames) {
                            Ok(row) => row.print(),
                            Err(e) => println!("| {label} | - | failed: {} | | | | | | |", e.replace('|', "/")),
                        },
                        Err(e) => println!("| {label} | - | unavailable: {} | | | | | | |", e.replace('|', "/")),
                    }
                }
            }
        }
    }

    pub fn main() {
        // SAFETY: raises the timer resolution of this process to 1 ms so the 60 fps pacing sleeps are accurate.
        unsafe { timeBeginPeriod(1) };
        if std::env::args().nth(1).as_deref() == Some("stage35") {
            stage35();
            return;
        }
        println!(
            "# Stage 13 measurements: {W}x{H} at {FPS} fps, {KBPS} kbps, Automatica, {FRAMES} frames paced (first {WARMUP} not counted)"
        );
        println!();
        println!(
            "Synthetic frames only. Latency in milliseconds; CPU is the process CPU time (user + kernel, all threads) as a percentage of one core while the paced run goes (GPU work is not CPU)."
        );
        println!();
        println!("## Adapters");
        let adapters = list_adapters().expect("adapters");
        for a in &adapters {
            println!("- {} (LUID {:#x}, vendor {:#06x}, software {})", a.name, a.id.0, a.vendor_id, a.software);
        }
        let cpu_frames = render_frames();
        println!();
        println!("## Encode (submit to output of the same frame)");
        println!();
        println!(
            "| path | input | median ms | p95 ms | max ms | of which convert ms (median) | CPU % of a core | output kbps | frames |"
        );
        println!("|---|---|---|---|---|---|---|---|---|");
        let probes = probe_encoders().expect("probe");
        let mut intel_stream: Option<Vec<EncodedFrame>> = None;
        for p in &probes {
            let label = format!("{} on {}", p.name, p.adapter.name);
            match &p.mft {
                Err(e) => println!("| {label} | - | unavailable: {} | | | | | | |", e.replace('|', "/")),
                Ok(m) => {
                    for gpu_input in [true, false] {
                        match encode_run(&label, &m.clsid, p.adapter.id, gpu_input, &cpu_frames) {
                            Ok((row, encoded)) => {
                                row.print();
                                if gpu_input && intel_stream.is_none() {
                                    intel_stream = Some(encoded);
                                }
                            }
                            Err(e) => println!("| {label} | - | failed: {} | | | | | | |", e.replace('|', "/")),
                        }
                    }
                }
            }
        }
        let stream = intel_stream.expect("at least one encoder works");
        println!();
        println!(
            "## Decode (access unit to picture), stream from the first working encoder, {} access units",
            stream.len()
        );
        println!();
        println!("| path | decoder | median ms | p95 ms | max ms | CPU % of a core | frames |");
        println!("|---|---|---|---|---|---|---|");
        for a in adapters.iter().filter(|a| !a.software) {
            for output in [DecoderOutput::Gpu, DecoderOutput::CpuBgra] {
                let label = format!("DXVA on {}", a.name);
                match decode_run(&label, a.id, true, output, &stream) {
                    Ok(row) => row.print_decode(),
                    Err(e) => println!("| {label} | failed: {} | | | | | | | |", e.replace('|', "/")),
                }
            }
        }
        let first = adapters.iter().find(|a| !a.software).expect("hardware adapter");
        match decode_run("software decode", first.id, false, DecoderOutput::CpuBgra, &stream) {
            Ok(row) => row.print_decode(),
            Err(e) => println!("| software decode | failed: {} | | | | | | | |", e.replace('|', "/")),
        }
    }
}

#[cfg(windows)]
fn main() {
    run::main();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("Windows only");
}
