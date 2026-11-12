//! Stage 35: the NVENC encoder (D21) on this PC's NVIDIA adapter, round trips through the DXVA decoder, the encoder choice
//! and the fallback, and a 30 s run on frames captured from the NVIDIA-owned display.
#![cfg(windows)]

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use swapdesk_codec_win::{
    AdapterDesc, ChosenBackend, DecoderOptions, EncoderBackend, EncoderOptions, H264Encoder, H264EncoderOptions,
    MfH264Decoder, NvencEncoder, NvencOptions, TextureUploader, VENDOR_NVIDIA, annexb, list_adapters,
};
use swapdesk_fakes::{BarcodeReader, FakeClock, SyntheticScreen};
use swapdesk_platform::{
    AdapterId, CaptureConfig, DecodedFrame, DisplayId, EncodedFrame, EncoderConfig, ErrorKind, Frame, GpuSurface,
    MonoMicros, Pixels, ScreenSource, VideoEncoder,
};
use swapdesk_proto::{Codec, H264Profile, Preset, VideoHeader};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Device, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

type Raw = Vec<(MonoMicros, Vec<u8>)>;

/// `n` synthetic frames at `w` x `h`, 1/60 s apart on a fake clock, counters from 0.
fn render(w: u32, h: u32, n: usize) -> Raw {
    let clock = FakeClock::new(MonoMicros(5_000_000_000));
    let mut screen = SyntheticScreen::new(w, h, 60).with_clock(Arc::new(clock.clone()));
    screen.open(DisplayId(0), CaptureConfig { max_fps: 60 }).expect("open synthetic screen");
    let mut out = Vec::new();
    for _ in 0..n {
        let f = screen.next_frame(Duration::from_secs(1)).expect("frame").expect("some frame");
        let Pixels::Cpu { data, .. } = f.pixels else { unreachable!("synthetic frames are CPU frames") };
        out.push((f.captured_at, data));
        clock.advance(Duration::from_micros(16_667));
    }
    out
}

const FRAMES: usize = 24;

fn raw(w: u32) -> &'static Raw {
    static HD: OnceLock<Raw> = OnceLock::new();
    static QHD: OnceLock<Raw> = OnceLock::new();
    match w {
        1920 => HD.get_or_init(|| render(1920, 1080, FRAMES)),
        2560 => QHD.get_or_init(|| render(2560, 1440, FRAMES)),
        _ => panic!("no frames for width {w}"),
    }
}

fn size(w: u32) -> (u32, u32) {
    if w == 1920 { (1920, 1080) } else { (2560, 1440) }
}

fn cpu_frame(w: u32, n: usize) -> Frame {
    let (width, height) = size(w);
    let (captured_at, data) = &raw(w)[n % FRAMES];
    Frame {
        display: DisplayId(0),
        captured_at: *captured_at,
        width,
        height,
        pixels: Pixels::Cpu { stride: width * 4, data: data.clone() },
        dirty: Vec::new(),
        content_changed: true,
    }
}

fn nvidia() -> Option<AdapterDesc> {
    list_adapters().expect("adapters").into_iter().find(|a| a.vendor_id == VENDOR_NVIDIA && !a.software)
}

fn intel() -> Option<AdapterDesc> {
    list_adapters().expect("adapters").into_iter().find(|a| a.vendor_id == 0x8086 && !a.software)
}

macro_rules! need_nvidia {
    () => {
        match nvidia() {
            Some(a) => a,
            None => {
                eprintln!("skipped: this PC has no NVIDIA adapter");
                return;
            }
        }
    };
}

fn config(w: u32, adapter: AdapterId, preset: Preset, fps: u16, kbps: u32) -> EncoderConfig {
    let (width, height) = size(w);
    EncoderConfig { width, height, fps, kbps, preset, adapter, codec: swapdesk_platform::Codec::H264 }
}

fn nvenc_on(uploader: &TextureUploader) -> NvencEncoder {
    NvencEncoder::new(NvencOptions { device: Some(uploader.device()), ..Default::default() })
}

fn header(e: &EncodedFrame, frame_id: u32) -> VideoHeader {
    VideoHeader {
        keyframe: e.keyframe,
        config: e.config_change,
        discontinuity: false,
        display_id: 0,
        frame_id,
        capture_us: e.captured_at.0,
        encode_us: e.encode_us,
        width: e.width,
        height: e.height,
        codec: Codec::H264,
        payload_len: e.annex_b.len() as u32,
    }
}

fn decode_all(dec: &mut MfH264Decoder, encoded: &[EncodedFrame]) -> Vec<DecodedFrame> {
    let mut out = Vec::new();
    for (id, e) in encoded.iter().enumerate() {
        if let Some(d) =
            dec.decode_settled(&header(e, id as u32), &e.annex_b).unwrap_or_else(|err| panic!("decode {id}: {err}"))
        {
            out.push(d);
        }
    }
    out
}

fn source_index(w: u32, e: &EncodedFrame) -> usize {
    (0..FRAMES)
        .find(|&i| raw(w)[i].0 == e.captured_at)
        .unwrap_or_else(|| panic!("unknown timestamp {:?}", e.captured_at))
}

fn assert_exact(w: u32, d: &DecodedFrame, index: usize, who: &str) {
    let reading = BarcodeReader::new()
        .read_decoded(d)
        .unwrap_or_else(|| panic!("{who}: barcode unreadable on frame {}", d.frame_id));
    assert_eq!(reading.counter, index as u32, "{who}: counter of frame {}", d.frame_id);
    assert_eq!(reading.captured_at, raw(w)[index].0, "{who}: timestamp of frame {}", d.frame_id);
    assert_eq!(d.captured_at, raw(w)[index].0, "{who}: header timestamp of frame {}", d.frame_id);
    assert_eq!((d.width, d.height), size(w), "{who}: size");
}

fn mean_bytes(frames: &[EncodedFrame]) -> f64 {
    frames.iter().map(|e| e.annex_b.len() as f64).sum::<f64>() / frames.len().max(1) as f64
}

fn roundtrip(w: u32, preset: Preset, gpu_input: bool) {
    let nv = need_nvidia!();
    let who =
        format!("NVENC {}x{} {preset:?} {}", size(w).0, size(w).1, if gpu_input { "texture in" } else { "CPU in" });
    let uploader = TextureUploader::new(nv.id).expect("uploader on the NVIDIA adapter");
    let mut enc = nvenc_on(&uploader);
    let info = enc.configure(&config(w, nv.id, preset, 60, 20_000)).unwrap_or_else(|e| panic!("{who}: configure: {e}"));
    assert!(info.hardware && info.low_latency && info.profile == H264Profile::High, "{who}: {info:?}");
    let mut encoded = Vec::new();
    for i in 0..16 {
        let f = cpu_frame(w, i);
        let f = if gpu_input { uploader.upload(&f).expect("upload") } else { f };
        encoded.extend(enc.encode(&f, false).unwrap_or_else(|e| panic!("{who}: encode {i}: {e}")));
        if gpu_input {
            assert!(!enc.input_copied(), "{who}: a texture with render-target binding must be encoded without a copy");
        }
    }
    encoded.extend(enc.flush().expect("flush"));
    assert_eq!(encoded.len(), 16, "{who}: one output per input");
    assert!(encoded[0].keyframe && encoded[0].config_change, "{who}: first frame is an IDR with config");
    for e in &encoded {
        assert!(e.annex_b.starts_with(&[0, 0, 0, 1]) || e.annex_b.starts_with(&[0, 0, 1]), "{who}: Annex B");
        if e.keyframe {
            assert!(
                annexb::contains(&e.annex_b, annexb::NAL_SPS) && annexb::contains(&e.annex_b, annexb::NAL_PPS),
                "{who}"
            );
        } else {
            assert!(!e.config_change);
        }
        assert_eq!((e.width as u32, e.height as u32), size(w));
    }
    assert_eq!(encoded.iter().filter(|e| e.keyframe).count(), 1, "{who}: infinite GOP, one IDR");
    let mut dec = MfH264Decoder::new(DecoderOptions { adapter: nv.id, ..Default::default() });
    let decoded = decode_all(&mut dec, &encoded);
    assert_eq!(decoded.len(), encoded.len(), "{who}: one picture per access unit");
    for (d, e) in decoded.iter().zip(&encoded) {
        assert_exact(w, d, source_index(w, e), &who);
    }
    println!(
        "{who}: {} frames, {} bytes, decoded through {:?}, encoder {:?}",
        decoded.len(),
        encoded.iter().map(|e| e.annex_b.len()).sum::<usize>(),
        dec.mft_name(),
        enc.summary()
    );
}

#[test]
fn barcode_round_trip_is_exact_through_nvenc_at_1080p() {
    roundtrip(1920, Preset::Auto, true);
    roundtrip(1920, Preset::Motion, true);
    roundtrip(1920, Preset::Auto, false);
}

#[test]
fn barcode_round_trip_is_exact_through_nvenc_at_1440p() {
    roundtrip(2560, Preset::Auto, true);
    roundtrip(2560, Preset::Motion, true);
}

#[test]
fn texto_nitido_keeps_every_other_frame_and_reads_back_exactly() {
    let nv = need_nvidia!();
    let uploader = TextureUploader::new(nv.id).expect("uploader");
    let mut enc = nvenc_on(&uploader);
    enc.configure(&config(1920, nv.id, Preset::Text, 60, 20_000)).expect("configure");
    assert_eq!(enc.effective(), Some((30, 40_000)));
    let mut encoded = Vec::new();
    for i in 0..FRAMES {
        encoded.extend(enc.encode(&uploader.upload(&cpu_frame(1920, i)).expect("upload"), false).expect("encode"));
    }
    assert!((11..=13).contains(&encoded.len()), "{} frames of {FRAMES} at 30 fps", encoded.len());
    let mut dec = MfH264Decoder::new(DecoderOptions { adapter: nv.id, ..Default::default() });
    let decoded = decode_all(&mut dec, &encoded);
    assert_eq!(decoded.len(), encoded.len());
    for (d, e) in decoded.iter().zip(&encoded) {
        assert_exact(1920, d, source_index(1920, e), "NVENC Texto nítido");
    }
    let kept: Vec<usize> = encoded.iter().map(|e| source_index(1920, e)).collect();
    assert!(kept.windows(2).all(|w| w[1] - w[0] == 2), "kept {kept:?}");
}

#[test]
fn a_forced_idr_arrives_within_one_frame() {
    let nv = need_nvidia!();
    let uploader = TextureUploader::new(nv.id).expect("uploader");
    let mut enc = nvenc_on(&uploader);
    enc.configure(&config(1920, nv.id, Preset::Auto, 60, 20_000)).expect("configure");
    let mut encoded = Vec::new();
    for i in 0..14 {
        let f = uploader.upload(&cpu_frame(1920, i)).expect("upload");
        encoded.extend(enc.encode(&f, i == 7).expect("encode"));
    }
    assert_eq!(encoded.len(), 14);
    let keys: Vec<usize> = encoded.iter().filter(|e| e.keyframe).map(|e| source_index(1920, e)).collect();
    assert_eq!(keys, vec![0, 7], "an IDR on the first frame and on the forced one, none else (infinite GOP)");
    assert!(!encoded[7].config_change || encoded[7].keyframe);
    for e in encoded.iter().filter(|e| e.keyframe) {
        assert!(annexb::contains(&e.annex_b, annexb::NAL_SPS), "SPS on every IDR");
        assert!(annexb::contains(&e.annex_b, annexb::NAL_PPS), "PPS on every IDR");
    }
    // The decoder takes the forced IDR in stride.
    let mut dec = MfH264Decoder::new(DecoderOptions { adapter: nv.id, ..Default::default() });
    for (d, e) in decode_all(&mut dec, &encoded).iter().zip(&encoded) {
        assert_exact(1920, d, source_index(1920, e), "forced IDR");
    }
}

#[test]
fn a_live_bitrate_change_takes_effect_without_a_new_idr() {
    let nv = need_nvidia!();
    for preset in [Preset::Auto, Preset::Motion] {
        let who = format!("{preset:?}");
        let uploader = TextureUploader::new(nv.id).expect("uploader");
        let mut enc = nvenc_on(&uploader);
        enc.configure(&config(1920, nv.id, preset, 60, 30_000)).expect("configure");
        let textures: Vec<Frame> = (0..FRAMES).map(|i| uploader.upload(&cpu_frame(1920, i)).expect("upload")).collect();
        let mut seq = 0usize;
        let mut run = |enc: &mut NvencEncoder, n: usize| {
            let mut out = Vec::new();
            for _ in 0..n {
                let t = &textures[seq % FRAMES];
                let Pixels::Gpu(s) = &t.pixels else { unreachable!() };
                let tex = s.0.downcast_ref::<ID3D11Texture2D>().expect("texture").clone();
                let f = Frame {
                    display: t.display,
                    captured_at: MonoMicros(5_000_000_000 + seq as u64 * 16_667),
                    width: t.width,
                    height: t.height,
                    pixels: Pixels::Gpu(GpuSurface(Box::new(tex))),
                    dirty: Vec::new(),
                    content_changed: true,
                };
                out.extend(enc.encode(&f, false).expect("encode"));
                seq += 1;
            }
            out
        };
        let high_frames = run(&mut enc, 96);
        let high = mean_bytes(&high_frames[32..]);
        enc.set_rate(2_000, 60).expect("set_rate down");
        let low_frames = run(&mut enc, 96);
        assert!(
            !low_frames.iter().any(|e| e.keyframe || e.config_change),
            "{who}: the rate change restarted the stream"
        );
        let low = mean_bytes(&low_frames[48..]);
        let kbps = |b: f64| b * 8.0 * 60.0 / 1000.0;
        println!("{who}: 30000 kbps asked -> {:.0} measured; 2000 asked -> {:.0} measured", kbps(high), kbps(low));
        assert!(low < high * 0.5, "{who}: {high:.0} bytes per frame before, {low:.0} after");
        assert_eq!(enc.effective(), Some((60, 2_000)));
        enc.set_rate(30_000, 60).expect("set_rate up");
        let up_frames = run(&mut enc, 96);
        let up = mean_bytes(&up_frames[48..]);
        assert!(up > low * 2.0, "{who}: {low:.0} bytes per frame low, {up:.0} after raising");
    }
}

#[test]
fn a_texture_without_the_render_target_bind_flag_still_encodes() {
    // A caller's texture with `D3D11_BIND_SHADER_RESOURCE` only (the capture crate's pooled textures also carry
    // `D3D11_BIND_RENDER_TARGET`, which the Media Foundation path's video processor needs).
    let nv = need_nvidia!();
    let uploader = TextureUploader::new(nv.id).expect("uploader");
    let device: ID3D11Device = uploader.device();
    let (w, h) = (1920u32, 1080u32);
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex = None;
    // SAFETY: valid descriptor and out pointer.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }.expect("texture");
    let tex = tex.expect("texture");
    let (_, data) = &raw(1920)[3];
    // SAFETY: `data` holds 1080 rows of 1920 BGRA pixels.
    unsafe {
        device.GetImmediateContext().expect("context").UpdateSubresource(&tex, 0, None, data.as_ptr().cast(), w * 4, 0);
    }
    let mut enc = nvenc_on(&uploader);
    enc.configure(&config(1920, nv.id, Preset::Auto, 60, 20_000)).expect("configure");
    let frame = |seq: u64| Frame {
        display: DisplayId(0),
        captured_at: MonoMicros(raw(1920)[3].0.0 + seq),
        width: w,
        height: h,
        pixels: Pixels::Gpu(GpuSurface(Box::new(tex.clone()))),
        dirty: Vec::new(),
        content_changed: true,
    };
    let first = enc.encode(&frame(0), false).expect("encode shader-resource-only texture");
    assert_eq!(first.len(), 1);
    println!("shader-resource-only capture-style texture: encoded with a GPU copy: {}", enc.input_copied());
    let mut encoded = first;
    for s in 1..6 {
        encoded.extend(enc.encode(&frame(s), false).expect("encode"));
    }
    let mut dec = MfH264Decoder::new(DecoderOptions { adapter: nv.id, ..Default::default() });
    let decoded = decode_all(&mut dec, &encoded);
    assert_eq!(decoded.len(), 6);
    let reading = BarcodeReader::new().read_decoded(&decoded[5]).expect("barcode");
    assert_eq!(reading.counter, 3);
}

#[test]
fn auto_picks_nvenc_on_the_nvidia_adapter_and_media_foundation_on_intel() {
    let nv = need_nvidia!();
    let uploader = TextureUploader::new(nv.id).expect("uploader");
    let mut enc = H264Encoder::new(H264EncoderOptions {
        mf: EncoderOptions { device: Some(uploader.device()), ..Default::default() },
        ..Default::default()
    });
    let info = enc.configure(&config(1920, nv.id, Preset::Auto, 60, 20_000)).expect("configure on NVIDIA");
    assert_eq!(enc.backend(), Some(ChosenBackend::Nvenc), "{info:?} {:?}", enc.fallback_reason());
    assert!(info.name.contains("NVENC"), "{info:?}");
    assert_eq!(enc.fallback_reason(), None);
    let out = enc.encode(&uploader.upload(&cpu_frame(1920, 0)).expect("upload"), false).expect("encode");
    assert_eq!(out.len(), 1);
    assert!(out[0].keyframe);
    // Without a device the encoder makes its own on the configured adapter and chooses the same way.
    let mut own = H264Encoder::new(H264EncoderOptions::default());
    own.configure(&config(1920, nv.id, Preset::Auto, 60, 20_000)).expect("configure own device");
    assert_eq!(own.backend(), Some(ChosenBackend::Nvenc));
    // Naming an MFT is a request for Media Foundation.
    let mut named = H264Encoder::new(H264EncoderOptions {
        mf: EncoderOptions { choice: swapdesk_codec_win::MftChoice::SoftwareOnly, ..Default::default() },
        ..Default::default()
    });
    named.configure(&config(1920, nv.id, Preset::Auto, 60, 20_000)).expect("software");
    assert_eq!(named.backend(), Some(ChosenBackend::MediaFoundation));

    match intel() {
        None => eprintln!("no Intel adapter on this PC: the Intel half is skipped"),
        Some(i) => {
            let up = TextureUploader::new(i.id).expect("uploader on Intel");
            let mut enc = H264Encoder::new(H264EncoderOptions {
                mf: EncoderOptions { device: Some(up.device()), ..Default::default() },
                ..Default::default()
            });
            let info = enc.configure(&config(1920, i.id, Preset::Auto, 60, 20_000)).expect("configure on Intel");
            assert_eq!(enc.backend(), Some(ChosenBackend::MediaFoundation), "{info:?}");
            assert!(!info.name.contains("NVENC"), "{info:?}");
            let out = enc.encode(&up.upload(&cpu_frame(1920, 0)).expect("upload"), false).expect("encode");
            assert_eq!(out.len(), 1);
            println!("Intel adapter -> {} ({})", info.name, if info.hardware { "hardware" } else { "software" });
            // And forcing NVENC on Intel is refused, not silently served.
            let mut forced = H264Encoder::new(H264EncoderOptions {
                backend: EncoderBackend::Nvenc,
                mf: EncoderOptions { device: Some(up.device()), ..Default::default() },
                ..Default::default()
            });
            let e = forced.configure(&config(1920, i.id, Preset::Auto, 60, 20_000)).expect_err("NVENC on Intel");
            assert_eq!(e.kind, ErrorKind::Unsupported, "{e}");
        }
    }
}

#[test]
fn a_missing_nvenc_library_falls_back_to_media_foundation() {
    let nv = need_nvidia!();
    let uploader = TextureUploader::new(nv.id).expect("uploader");
    // NVENC alone reports it.
    let mut alone = NvencEncoder::new(NvencOptions {
        device: Some(uploader.device()),
        library: Some("swapdesk-wrong-nvenc-name.dll".to_string()),
        ..Default::default()
    });
    let e = alone.configure(&config(1920, nv.id, Preset::Auto, 60, 20_000)).expect_err("wrong library name");
    assert_eq!(e.kind, ErrorKind::Unavailable, "{e}");
    // Auto goes on with Media Foundation.
    let mut enc = H264Encoder::new(H264EncoderOptions {
        nvenc_library: Some("swapdesk-wrong-nvenc-name.dll".to_string()),
        mf: EncoderOptions { device: Some(uploader.device()), ..Default::default() },
        ..Default::default()
    });
    let info = enc.configure(&config(1920, nv.id, Preset::Auto, 60, 20_000)).expect("fallback configure");
    assert_eq!(enc.backend(), Some(ChosenBackend::MediaFoundation), "{info:?}");
    let reason = enc.fallback_reason().expect("a reason is kept");
    assert!(reason.contains("swapdesk-wrong-nvenc-name.dll"), "{reason}");
    assert!(!info.name.contains("NVENC"));
    let out =
        enc.encode(&uploader.upload(&cpu_frame(1920, 0)).expect("upload"), false).expect("encode through the fallback");
    assert_eq!(out.len(), 1);
    // The forced backend does not fall back.
    let mut forced = H264Encoder::new(H264EncoderOptions {
        backend: EncoderBackend::Nvenc,
        nvenc_library: Some("swapdesk-wrong-nvenc-name.dll".to_string()),
        ..Default::default()
    });
    assert_eq!(
        forced.configure(&config(1920, nv.id, Preset::Auto, 60, 20_000)).expect_err("forced NVENC, wrong library").kind,
        ErrorKind::Unavailable
    );
}

#[test]
fn the_encoder_refuses_bad_configurations_and_unconfigured_use() {
    let nv = need_nvidia!();
    let mut enc = NvencEncoder::new(NvencOptions::default());
    let mut cfg = config(1920, nv.id, Preset::Auto, 60, 8_000);
    cfg.width = 1921;
    assert_eq!(enc.configure(&cfg).expect_err("odd width").kind, ErrorKind::InvalidInput);
    cfg.width = 1920;
    cfg.fps = 0;
    assert_eq!(enc.configure(&cfg).expect_err("zero fps").kind, ErrorKind::InvalidInput);
    let mut fresh = NvencEncoder::new(NvencOptions::default());
    assert_eq!(fresh.encode(&cpu_frame(1920, 0), false).expect_err("not configured").kind, ErrorKind::Closed);
    assert_eq!(fresh.set_rate(1000, 30).expect_err("not configured").kind, ErrorKind::Closed);
    assert!(fresh.flush().expect("flush of nothing").is_empty());
    // A display size change restarts the session: new SPS/PPS and an IDR with config_change.
    let uploader = TextureUploader::new(nv.id).expect("uploader");
    let mut enc = nvenc_on(&uploader);
    enc.configure(&config(1920, nv.id, Preset::Auto, 60, 20_000)).expect("configure");
    enc.encode(&uploader.upload(&cpu_frame(1920, 0)).expect("upload"), false).expect("first size");
    let out = enc.encode(&uploader.upload(&cpu_frame(2560, 0)).expect("upload"), false).expect("second size");
    assert!(out[0].keyframe && out[0].config_change);
    assert_eq!((out[0].width, out[0].height), (2560, 1440));
}

/// Frames captured from the NVIDIA-owned display through DXGI Desktop Duplication, encoded for 30 s paced at 60 fps. The
/// frames are never saved or printed; only counts and sizes are asserted. DDA delivers a frame only when the desktop
/// changes, so a timeout repeats the last captured texture (what the video sender does).
#[test]
fn thirty_seconds_of_real_capture_of_the_nvidia_display_encode_at_60_fps() {
    use swapdesk_capture_win::{DxgiScreen, gpu_payload};
    let nv = need_nvidia!();
    let mut screen = DxgiScreen::new();
    let displays = screen.displays().expect("displays");
    let Some(display) = displays.iter().find(|d| d.adapter == nv.id) else {
        eprintln!("skipped: no display is owned by the NVIDIA adapter");
        return;
    };
    screen.open(display.id, CaptureConfig { max_fps: 60 }).expect("open the NVIDIA display");
    // The first frame names the device and the size.
    let first = loop {
        match screen.next_frame(Duration::from_millis(500)) {
            Ok(Some(f)) => break f,
            Ok(None) => {}
            Err(e) => panic!("capture: {e}"),
        }
    };
    let gf = gpu_payload(&first).expect("GPU frame");
    let device = gf.device().device.clone();
    let (w, h) = (first.width, first.height);
    let mut enc = H264Encoder::new(H264EncoderOptions {
        mf: EncoderOptions { device: Some(device), ..Default::default() },
        ..Default::default()
    });
    enc.configure(&EncoderConfig {
        width: w,
        height: h,
        fps: 60,
        kbps: 30_000,
        preset: Preset::Auto,
        adapter: nv.id,
        codec: swapdesk_platform::Codec::H264,
    })
    .expect("configure on the capture device");
    assert_eq!(enc.backend(), Some(ChosenBackend::Nvenc), "{:?}", enc.fallback_reason());

    let mut current = first;
    let mut fresh = 1u64;
    let mut sizes: Vec<usize> = Vec::new();
    let mut keyframes = 0u32;
    let start = Instant::now();
    let total = 1800u64;
    for seq in 0..total {
        let due = start + Duration::from_nanos(1_000_000_000 / 60 * seq);
        // Take a newer capture when one is waiting, without outwaiting this frame's slot.
        let wait = due.saturating_duration_since(Instant::now());
        match screen.next_frame(wait.min(Duration::from_millis(8))) {
            Ok(Some(f)) => {
                current = f;
                fresh += 1;
            }
            Ok(None) => {}
            Err(e) if e.kind == ErrorKind::AccessLost => {
                screen.reacquire().expect("reacquire");
            }
            Err(e) => panic!("capture: {e}"),
        }
        let gf = gpu_payload(&current).expect("GPU frame");
        let tex = gf.texture().clone();
        let frame = Frame {
            display: current.display,
            captured_at: MonoMicros(1_000_000 + seq * 16_667),
            width: current.width,
            height: current.height,
            pixels: Pixels::Gpu(GpuSurface(Box::new(tex))),
            dirty: Vec::new(),
            content_changed: true,
        };
        for e in enc.encode(&frame, false).expect("encode a captured frame") {
            assert_eq!((e.width as u32, e.height as u32), (w, h));
            keyframes += u32::from(e.keyframe);
            sizes.push(e.annex_b.len());
        }
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        }
    }
    let secs = start.elapsed().as_secs_f64();
    let fps = sizes.len() as f64 / secs;
    let mean = sizes.iter().sum::<usize>() as f64 / sizes.len().max(1) as f64;
    println!(
        "real capture {w}x{h}: {} frames in {secs:.1} s = {fps:.1} fps ({fresh} of them freshly captured), {keyframes} IDR, mean {mean:.0} bytes, max {} bytes",
        sizes.len(),
        sizes.iter().max().copied().unwrap_or(0)
    );
    assert_eq!(sizes.len() as u64, total, "one output per input");
    assert!(fps > 55.0 && fps < 62.0, "{fps:.1} fps");
    assert_eq!(keyframes, 1, "infinite GOP: only the first frame is an IDR");
    assert!(sizes.iter().all(|&s| s > 0 && s < 4_000_000), "frame sizes out of range");
}
