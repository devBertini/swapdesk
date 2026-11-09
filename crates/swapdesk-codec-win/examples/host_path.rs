//! What the host's video path does on the display it would stream: the encoder `swapdesk-helper` builds for it
//! (`H264Encoder`, backend Auto, the capture device), the family and MFT it lands on for each viewer preset, and what goes
//! out at 60 fps while the rate control raises the bitrate by 5 % every second (`RateConfig` defaults: start 20 Mbps,
//! ceiling 80 Mbps), as `SetRate` does in a session. The Media Foundation backend rows show the path a failed NVENC
//! open falls back to.
//!
//! Two sources: "screen" encodes the capture texture of the display as the helper does (DXGI Desktop Duplication; a
//! timeout repeats the last texture, which is what a still desktop costs), "motion" encodes synthetic frames of the same
//! size (C7) uploaded to the capture device, so the content moves every frame. Nothing captured is saved or printed;
//! only counts, sizes and times.
//!
//! Run: `cargo run -p swapdesk-codec-win --example host_path [seconds per row]`.

#[cfg(windows)]
mod run {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use swapdesk_capture_win::{DxgiScreen, gpu_payload};
    use swapdesk_codec_win::{
        ChosenBackend, EncoderBackend, EncoderOptions, H264Encoder, H264EncoderOptions, list_adapters,
    };
    use swapdesk_fakes::{FakeClock, SyntheticScreen};
    use swapdesk_platform::{
        CaptureConfig, DisplayId, EncoderConfig, ErrorKind, Frame, GpuSurface, MonoMicros, Pixels, ScreenSource,
        VideoEncoder,
    };
    use swapdesk_proto::Preset;
    use windows::Win32::Media::timeBeginPeriod;

    const FPS: u64 = 60;
    const START_KBPS: u32 = 20_000;
    const MAX_KBPS: u32 = 80_000;

    fn percentile(sorted: &[u32], p: f64) -> u32 {
        if sorted.is_empty() {
            return 0;
        }
        sorted[((sorted.len() - 1) as f64 * p).round() as usize]
    }

    fn render(w: u32, h: u32, n: usize) -> Vec<Vec<u8>> {
        let clock = FakeClock::new(MonoMicros(5_000_000_000));
        let mut screen = SyntheticScreen::new(w, h, 60).with_clock(Arc::new(clock.clone()));
        screen.open(DisplayId(0), CaptureConfig { max_fps: 60 }).expect("open synthetic screen");
        (0..n)
            .map(|_| {
                let f = screen.next_frame(Duration::from_secs(1)).expect("frame").expect("some frame");
                clock.advance(Duration::from_micros(16_667));
                let Pixels::Cpu { data, .. } = f.pixels else { unreachable!("synthetic frames are CPU frames") };
                data
            })
            .collect()
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Source {
        Screen,
        Motion,
    }

    pub fn main() {
        // SAFETY: raises the timer resolution for this process, as the other measurements do.
        unsafe { timeBeginPeriod(1) };
        let seconds: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(6);
        let adapters = list_adapters().expect("adapters");
        for a in &adapters {
            println!("adapter {:#x}: {} vendor {:#06x} software {}", a.id.0, a.name, a.vendor_id, a.software);
        }
        let mut screen = DxgiScreen::new();
        let displays = screen.displays().expect("displays");
        for d in &displays {
            println!(
                "display {} {}: {}x{} scale {}% primary {} adapter {:#x}",
                d.id.0,
                d.name,
                d.bounds.width,
                d.bounds.height,
                d.scale_milli / 10,
                d.primary,
                d.adapter.0
            );
        }
        let display = displays.iter().find(|d| d.primary).or(displays.first()).expect("a display").clone();
        screen.open(display.id, CaptureConfig { max_fps: 60 }).expect("open the display");
        let first = loop {
            match screen.next_frame(Duration::from_millis(500)) {
                Ok(Some(f)) => break f,
                Ok(None) => {}
                Err(e) => panic!("capture: {e}"),
            }
        };
        let device = gpu_payload(&first).expect("GPU frame").device().device.clone();
        let (w, h) = (first.width, first.height);
        println!("streaming display {} ({w}x{h}); {seconds} s per row, {FPS} fps paced\n", display.name);
        let synthetic = render(w, h, 32);
        let mut current = first;

        println!(
            "| backend asked | preset | source | chosen | encoder | fallback | out/in | fps | encode median ms | p95 ms | max ms | Mbps out | KB/frame | keyframes | config changes | errors |"
        );
        println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
        for backend in [EncoderBackend::Auto, EncoderBackend::MediaFoundation] {
            if seconds == 0 {
                break;
            }
            for preset in [Preset::Auto, Preset::Text, Preset::Motion] {
                for source in [Source::Screen, Source::Motion] {
                    if backend == EncoderBackend::MediaFoundation && source == Source::Screen {
                        continue;
                    }
                    let mut enc = H264Encoder::new(H264EncoderOptions {
                        backend,
                        mf: EncoderOptions { device: Some(device.clone()), ..Default::default() },
                        ..Default::default()
                    });
                    let cfg = EncoderConfig {
                        width: w,
                        height: h,
                        fps: FPS as u16,
                        kbps: START_KBPS,
                        preset,
                        adapter: display.adapter,
                    };
                    let row = format!(
                        "| {backend:?} | {preset:?} | {} |",
                        if source == Source::Screen { "screen" } else { "motion" }
                    );
                    let info = match enc.configure(&cfg) {
                        Ok(i) => i,
                        Err(e) => {
                            println!("{row} configure failed | {e} | | | | | | | | | | | |");
                            continue;
                        }
                    };
                    let chosen = match enc.backend() {
                        Some(ChosenBackend::Nvenc) => "NVENC",
                        Some(ChosenBackend::MediaFoundation) => "MF",
                        None => "-",
                    };
                    let mf_name = enc.mf().and_then(|m| m.mft()).map(|s| s.name.clone());
                    let name = mf_name.unwrap_or_else(|| info.name.clone());
                    let fallback = enc.fallback_reason().unwrap_or("-").replace('|', "/");

                    let total = seconds * FPS;
                    let mut times = Vec::new();
                    let mut bytes = 0usize;
                    let (mut outs, mut keys, mut configs, mut errors) = (0u64, 0u32, 0u32, Vec::<String>::new());
                    let mut kbps = START_KBPS;
                    let start = Instant::now();
                    for seq in 0..total {
                        let due = start + Duration::from_nanos(1_000_000_000 / FPS * seq);
                        if seq > 0 && seq % FPS == 0 && kbps < MAX_KBPS {
                            kbps = ((f64::from(kbps) * 1.05).round() as u32).min(MAX_KBPS);
                            if let Err(e) = enc.set_rate(kbps, FPS as u16) {
                                errors.push(format!("set_rate {kbps}: {e}"));
                            }
                        }
                        let captured_at = MonoMicros(1_000_000 + seq * 16_667);
                        let frame = match source {
                            Source::Screen => {
                                let wait = due.saturating_duration_since(Instant::now()).min(Duration::from_millis(8));
                                match screen.next_frame(wait) {
                                    Ok(Some(f)) => current = f,
                                    Ok(None) => {}
                                    Err(e) if e.kind == ErrorKind::AccessLost => screen.reacquire().expect("reacquire"),
                                    Err(e) => panic!("capture: {e}"),
                                }
                                let tex = gpu_payload(&current).expect("GPU frame").texture().clone();
                                Frame {
                                    display: current.display,
                                    captured_at,
                                    width: w,
                                    height: h,
                                    pixels: Pixels::Gpu(GpuSurface(Box::new(tex))),
                                    dirty: Vec::new(),
                                    content_changed: true,
                                }
                            }
                            Source::Motion => Frame {
                                display: DisplayId(0),
                                captured_at,
                                width: w,
                                height: h,
                                pixels: Pixels::Cpu {
                                    stride: w * 4,
                                    data: synthetic[seq as usize % synthetic.len()].clone(),
                                },
                                dirty: Vec::new(),
                                content_changed: true,
                            },
                        };
                        let t0 = Instant::now();
                        match enc.encode(&frame, seq == 0) {
                            Ok(out) => {
                                for e in out {
                                    outs += 1;
                                    bytes += e.annex_b.len();
                                    keys += u32::from(e.keyframe);
                                    configs += u32::from(e.config_change);
                                }
                            }
                            Err(e) => errors.push(format!("encode {seq}: {e}")),
                        }
                        times.push(t0.elapsed().as_micros().min(u128::from(u32::MAX)) as u32);
                        let now = Instant::now();
                        if due > now {
                            std::thread::sleep(due - now);
                        }
                    }
                    let secs = start.elapsed().as_secs_f64();
                    times.sort_unstable();
                    let expected_in = match preset {
                        Preset::Text => total / 2,
                        _ => total,
                    };
                    errors.truncate(2);
                    println!(
                        "{row} {chosen} | {name} | {fallback} | {outs}/{expected_in} | {:.1} | {:.2} | {:.2} | {:.2} | {:.1} | {:.1} | {keys} | {configs} | {} |",
                        outs as f64 / secs,
                        f64::from(percentile(&times, 0.5)) / 1000.0,
                        f64::from(percentile(&times, 0.95)) / 1000.0,
                        f64::from(times.last().copied().unwrap_or(0)) / 1000.0,
                        bytes as f64 * 8.0 / secs / 1e6,
                        bytes as f64 / outs.max(1) as f64 / 1024.0,
                        errors.join("; ").replace('|', "/")
                    );
                }
            }
        }
        refine_rows(&device, display.adapter, w, h, &mut screen, &mut current);
    }

    /// A page of text-like strokes (the still-screen refinement test's page), black on white, hard edges.
    fn page(w: u32, h: u32, n: usize) -> Vec<u8> {
        let scroll = n * 3;
        let mut data = vec![240u8; (w * h * 4) as usize];
        for y in 0..h as usize {
            let sy = y + scroll;
            let (row, cy) = (sy / 22, sy % 22);
            for x in 0..w as usize {
                let (col, cx) = (x / 11, x % 11);
                let k = (row as u32).wrapping_mul(0x9E37_79B1) ^ (col as u32).wrapping_mul(0x85EB_CA6B);
                let k = (k ^ (k >> 15)).wrapping_mul(0x2C1B_3C6D);
                let k = k ^ (k >> 12);
                if k & 7 == 0 {
                    continue;
                }
                let bar_h = (4 + (k >> 4) % 3) as usize..(6 + (k >> 4) % 3) as usize;
                let bar_v = (2 + (k >> 8) % 4) as usize..(4 + (k >> 8) % 4) as usize;
                let ink = (k >> 12) & 3;
                let hit = (ink != 3 && bar_h.contains(&cy) && (1..10).contains(&cx))
                    || (ink != 2 && bar_v.contains(&cx) && (3..20).contains(&cy))
                    || (ink == 1 && cx == 8 && (3..14).contains(&cy));
                if hit {
                    let o = (y * w as usize + x) * 4;
                    data[o..o + 3].copy_from_slice(&[20, 20, 20]);
                }
            }
        }
        data
    }

    /// What one still-screen refinement (L4, sent 400 ms after the screen stops) costs: the frame size and the time, on
    /// the real screen through the helper's encoder, and on a scrolled text page through every hardware path.
    fn refine_rows(
        device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
        adapter: swapdesk_platform::AdapterId,
        w: u32,
        h: u32,
        screen: &mut DxgiScreen,
        current: &mut Frame,
    ) {
        println!("\n| refinement | encoder | normal KB/frame | refinement KB | refinement ms | next frame KB (key?) |");
        println!("|---|---|---|---|---|---|");
        let pages: Vec<Vec<u8>> = (0..31).map(|n| page(w, h, n)).collect();
        let cpu = |n: usize| Frame {
            display: DisplayId(0),
            captured_at: MonoMicros(1_000_000 + n as u64 * 16_667),
            width: w,
            height: h,
            pixels: Pixels::Cpu { stride: w * 4, data: pages[n].clone() },
            dirty: Vec::new(),
            content_changed: true,
        };
        let intel = list_adapters()
            .expect("adapters")
            .into_iter()
            .find(|a| !a.software && a.vendor_id != swapdesk_codec_win::VENDOR_NVIDIA);
        let mut runs: Vec<(&str, H264EncoderOptions, swapdesk_platform::AdapterId)> = vec![
            (
                "real screen",
                H264EncoderOptions {
                    mf: EncoderOptions { device: Some(device.clone()), ..Default::default() },
                    ..Default::default()
                },
                adapter,
            ),
            (
                "text page",
                H264EncoderOptions {
                    mf: EncoderOptions { device: Some(device.clone()), ..Default::default() },
                    ..Default::default()
                },
                adapter,
            ),
        ];
        if let Some(i) = &intel {
            runs.push(("text page", H264EncoderOptions::default(), i.id));
        }
        for (label, options, on) in runs {
            let mut enc = H264Encoder::new(options);
            let cfg =
                EncoderConfig { width: w, height: h, fps: 60, kbps: START_KBPS, preset: Preset::Auto, adapter: on };
            let info = match enc.configure(&cfg) {
                Ok(i) => i,
                Err(e) => {
                    println!("| {label} | configure failed: {e} | | | | |");
                    continue;
                }
            };
            let mut normal = Vec::new();
            let mut last = None;
            for n in 0..30usize {
                let f = if label == "real screen" {
                    if let Ok(Some(f)) = screen.next_frame(Duration::from_millis(16)) {
                        *current = f;
                    }
                    let tex = gpu_payload(current).expect("GPU frame").texture().clone();
                    Frame {
                        display: current.display,
                        captured_at: MonoMicros(1_000_000 + n as u64 * 16_667),
                        width: w,
                        height: h,
                        pixels: Pixels::Gpu(GpuSurface(Box::new(tex))),
                        dirty: Vec::new(),
                        content_changed: true,
                    }
                } else {
                    cpu(n)
                };
                normal.extend(enc.encode(&f, n == 0).expect("encode"));
                last = Some(f);
            }
            let last = last.expect("frames");
            let mean = normal.iter().skip(1).map(|e| e.annex_b.len()).sum::<usize>() as f64
                / (normal.len().saturating_sub(1)).max(1) as f64;
            let t0 = Instant::now();
            let refined = enc.refine(&last);
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            let next_frame = if label == "real screen" {
                Frame { captured_at: MonoMicros(1_000_000 + 31 * 16_667), ..clone_gpu(&last) }
            } else {
                cpu(30)
            };
            let next = enc.encode(&next_frame, false).unwrap_or_default();
            match refined {
                Ok(r) => println!(
                    "| {label} | {} | {:.1} | {:.1} | {ms:.1} | {:.1} ({}) |",
                    info.name,
                    mean / 1024.0,
                    r.iter().map(|e| e.annex_b.len()).sum::<usize>() as f64 / 1024.0,
                    next.iter().map(|e| e.annex_b.len()).sum::<usize>() as f64 / 1024.0,
                    next.iter().any(|e| e.keyframe)
                ),
                Err(e) => println!("| {label} | {} | {:.1} | refused: {e} | {ms:.1} | |", info.name, mean / 1024.0),
            }
        }
    }

    fn clone_gpu(f: &Frame) -> Frame {
        let Pixels::Gpu(s) = &f.pixels else { unreachable!("GPU frame") };
        let tex = s.0.downcast_ref::<windows::Win32::Graphics::Direct3D11::ID3D11Texture2D>().expect("texture").clone();
        Frame {
            display: f.display,
            captured_at: f.captured_at,
            width: f.width,
            height: f.height,
            pixels: Pixels::Gpu(GpuSurface(Box::new(tex))),
            dirty: Vec::new(),
            content_changed: true,
        }
    }
}

#[cfg(windows)]
fn main() {
    run::main();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("host_path runs on Windows only");
}
