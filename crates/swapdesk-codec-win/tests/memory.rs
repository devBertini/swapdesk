//! Memory of the decoder and of encoder reconfiguration (stage 38): private bytes and kernel handles must stay flat
//! over thousands of frames and dozens of reconfigurations. The D3D11 debug layer (`ReportLiveDeviceObjects`) is not
//! installed on this PC (no `D3D11*SDKLayers.dll`), so the measure is the process's private bytes and handle count.
#![cfg(windows)]

mod common;

use common::*;
use swapdesk_codec_win::{DecoderOptions, DecoderOutput, MfH264Decoder};
use swapdesk_platform::{DecodedFrame, VideoEncoder};

const FRAMES: usize = 3000;
const WARM: usize = 300;
/// Growth allowed between the warm-up and the end (allocator noise, not a leak: a leak is 0.17 MB/s at 60 fps).
const BUDGET_MB: f64 = 2.0;

fn hardware_first() -> Candidate {
    candidates().iter().find(|c| c.hardware).cloned().unwrap_or_else(software_candidate)
}

/// Decodes `FRAMES` pictures (the 64 access units over and over, each loop starting from its keyframe) and returns the
/// private bytes and handles at frame `WARM` and at the end. `hold` keeps the last picture alive, as a renderer does.
fn decode_run(output: DecoderOutput, hold: bool) -> ((u64, u32), (u64, u32), usize) {
    let c = hardware_first();
    let stream = encode_stream(&c);
    assert!(stream[0].keyframe, "{}: the stream starts with a keyframe", c.name);
    let mut dec = MfH264Decoder::new(DecoderOptions { output, adapter: c.adapter, ..Default::default() });
    let mut at_warm = (0, 0);
    let mut last: Option<DecodedFrame> = None;
    let mut pictures = 0;
    for n in 0..FRAMES {
        let e = &stream[n % stream.len()];
        let got =
            dec.decode_settled(&header(e, n as u32), &e.annex_b).unwrap_or_else(|err| panic!("decode {n}: {err}"));
        if let Some(d) = got {
            pictures += 1;
            if hold {
                last = Some(d);
            }
        }
        if n + 1 == WARM {
            at_warm = (private_bytes(), handle_count());
        }
    }
    drop(last);
    let end = (private_bytes(), handle_count());
    println!(
        "{} {output:?}: {pictures} pictures of {FRAMES}; private {:.1} MB -> {:.1} MB, handles {} -> {}",
        c.name,
        mb(at_warm.0),
        mb(end.0),
        at_warm.1,
        end.1
    );
    (at_warm, end, pictures)
}

fn assert_flat(at_warm: (u64, u32), end: (u64, u32)) {
    let grown = mb(end.0) - mb(at_warm.0);
    assert!(
        grown <= BUDGET_MB,
        "private bytes grew {grown:.1} MB between frame {WARM} and {FRAMES} (budget {BUDGET_MB} MB)"
    );
    assert!(end.1 <= at_warm.1 + 8, "handles grew from {} to {}", at_warm.1, end.1);
}

/// Measurement, not a gate: 3000 frames take 5 to 15 s. Run it alone, serially:
/// `cargo nextest run -p swapdesk-codec-win --run-ignored only --test-threads 1 -E 'test(gpu_output_decoding_keeps_private_bytes_flat)'`
#[test]
#[ignore = "measure: run alone, serially"]
fn gpu_output_decoding_keeps_private_bytes_flat() {
    let (warm, end, pictures) = decode_run(DecoderOutput::Gpu, true);
    assert!(pictures >= FRAMES - 2, "{pictures} pictures");
    assert_flat(warm, end);
}

/// Waits for the asynchronous releases of the MFT and the driver, then reads the counters.
fn settled() -> (u64, u32) {
    std::thread::sleep(std::time::Duration::from_millis(500));
    (private_bytes(), handle_count())
}

/// Fifty reconfigurations of every encoder this PC offers (a window resize or a rate change that rebuilds the MFT):
/// the MFT, its GPU session and the event callback of an asynchronous MFT must all be released. The counters are read
/// with no encoder alive, before and after, so the one encoder in use at the end does not count.
///
/// Measurement, not a gate: it takes 25 s and its private-bytes baseline moves by 100 MB when other processes use Quick
/// Sync. Run it alone, serially:
/// `cargo nextest run -p swapdesk-codec-win --run-ignored only --test-threads 1 -E 'test(fifty_encoder_reconfigurations_do_not_leak)'`
#[test]
#[ignore = "measure: run alone, serially"]
fn fifty_encoder_reconfigurations_do_not_leak() {
    for c in candidates() {
        let cfg = config(c, swapdesk_proto::Preset::Auto, 60, 20_000);
        let cycle = |n: usize| {
            let mut enc = encoder(c);
            for i in 0..n {
                enc.configure(&cfg).unwrap_or_else(|e| panic!("{} reconfigure {i}: {e}", c.name));
                for k in 0..3 {
                    enc.encode(&frame(k), false).expect("encode");
                }
            }
        };
        // Settle the driver's pools and the MF work queues first.
        cycle(4);
        let (p0, h0) = settled();
        cycle(50);
        let (p1, h1) = settled();
        println!(
            "{} (hardware {}): 50 reconfigures, private {:.1} MB -> {:.1} MB, handles {h0} -> {h1}",
            c.name,
            c.hardware,
            mb(p0),
            mb(p1)
        );
        // A leaked hardware MFT was 107 MB per reconfiguration; 16 MB over fifty is allocator and driver noise. The software
        // MFT is left out of the bytes check: its multi-hundred-MB heap is returned to the OS when the allocator decides
        // (identical runs moved by -113 MB and +98 MB), so only its handles count.
        if c.hardware {
            assert!(
                mb(p1) - mb(p0) <= 16.0,
                "{}: private bytes grew {:.1} MB over 50 reconfigurations",
                c.name,
                mb(p1) - mb(p0)
            );
        }
        // Left over by the activation of a hardware MFT itself (measured in isolation: 3 handles per `ActivateObject` of the
        // Quick Sync MFT, with or without our shutdown); a leaked MFT and its callback was 30 handles and 100 MB.
        assert!(h1 <= h0 + 4 * 50, "{}: handles {h0} -> {h1}", c.name);
    }
}

/// The quick guard of the default gate: ten reconfigurations and ten refused configurations of a hardware encoder leave
/// no more than the handles of the MFT's own activation (about 3 each). Handles are per process, so other tests using
/// the same GPU do not disturb the count, and the bound is generous: a leaked MFT and its callback were 30 each.
/// (The refused configuration is 240 fps, which the Quick Sync MFT rejects at `SetOutputType`, after activation; a
/// leak by the error path itself could not be told apart here, because that MFT frees itself either way.)
#[test]
fn reconfiguring_and_refused_configurations_leave_no_handles_behind() {
    for c in candidates().iter().filter(|c| c.hardware) {
        let cfg = config(c, swapdesk_proto::Preset::Auto, 60, 20_000);
        let mut refused = config(c, swapdesk_proto::Preset::Auto, 240, 20_000);
        refused.width = 1920;
        let cycle = |n: usize| {
            let mut enc = encoder(c);
            for i in 0..n {
                enc.configure(&cfg).unwrap_or_else(|e| panic!("{} reconfigure {i}: {e}", c.name));
                let _ = enc.configure(&refused);
            }
        };
        cycle(2);
        let (_, h0) = settled();
        cycle(10);
        let (_, h1) = settled();
        println!("{}: 10 reconfigures and 10 refusals, handles {h0} -> {h1}", c.name);
        // Two activations per round, about 3 handles each left by the driver: 60; a leaked MFT per round is 300+.
        assert!(h1 <= h0 + 6 * 10 + 20, "{}: handles {h0} -> {h1}", c.name);
    }
}
