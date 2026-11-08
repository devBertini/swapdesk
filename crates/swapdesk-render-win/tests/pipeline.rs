//! SyntheticScreen -> codec-win encoder -> decoder -> renderer in a window of the test's own, measured at the presented
//! frame (stage 15).
//!
//! The normal suite runs a 3 s pipeline that checks integrity (every frame arrives, decodes, presents and carries its
//! barcode). The budgets B1 and B2 depend on the GPU being free, and the workspace suite runs the codec tests on the same
//! GPU in parallel (a 10 s run under that load measured 266 ms median), so the budget runs are ignored tests for an idle
//! PC: `cargo nextest run -p swapdesk-render-win --run-ignored only`. The 10-minute run is `examples/b1b2.rs`.
#![cfg(windows)]

#[path = "common/harness.rs"]
mod harness;

use harness::{Config, Report, percentile, run};

fn integrity(r: &Report) {
    println!("{}", r.text());
    assert!(r.viewed.render_errors.is_empty(), "errors: {:?}", r.viewed.render_errors);
    assert_eq!(r.viewed.decode_errors, 0, "decode errors");
    assert_eq!(r.viewed.unreadable, 0, "barcodes the presented frames did not carry");
    assert_eq!(r.viewed.dropped_by_renderer, 0, "frames the renderer dropped");
    assert_eq!(r.lost(), 0, "frames that never reached the screen");
    assert_eq!(r.produced.encoded, r.produced.captured, "one encoded frame per captured frame");
    // Counters come out in order, once each.
    let counters: Vec<u32> = r.viewed.samples.iter().map(|s| s.counter).collect();
    assert!(counters.windows(2).all(|w| w[1] == w[0] + 1), "counters out of order");
    // Every presented frame was captured before it was presented, and nothing is stuck.
    assert!(r.viewed.samples.iter().all(|s| s.presented_at > s.captured_at));
    assert!(
        percentile(&r.latencies_ms(), 0.5) < 1000.0,
        "pipeline stuck: median {:.0} ms",
        percentile(&r.latencies_ms(), 0.5)
    );
}

fn budgets(r: &Report, seconds: f64) {
    integrity(r);
    let lat = r.latencies_ms();
    // B1: median at most 40 ms, p95 at most 60 ms.
    assert!(percentile(&lat, 0.5) <= 40.0, "B1 median {:.2} ms", percentile(&lat, 0.5));
    assert!(percentile(&lat, 0.95) <= 60.0, "B1 p95 {:.2} ms", percentile(&lat, 0.95));
    // B2: median of the 1 s windows at least 59 presented frames, at most 0.5 % of the frames lost.
    let mut fps = r.fps_windows();
    fps.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    assert!(fps.len() as f64 >= seconds - 3.0, "only {} full windows", fps.len());
    assert!(percentile(&fps, 0.5) >= 59.0, "B2 median fps {:.1}", percentile(&fps, 0.5));
    assert!(r.lost_percent() <= 0.5, "B2 lost {} frames ({:.3} %)", r.lost(), r.lost_percent());
}

/// 3 s of 1080p60 on the shell's device (the integrated GPU, as `Gfx::new` chooses): every frame arrives intact.
#[test]
fn three_seconds_of_1080p60_present_every_frame_intact() {
    let r = run(&Config::b1b2(3.0), None).expect("run");
    integrity(&r);
}

/// B1 and B2 over 10 s of 1080p60 on the shell's device. Needs the GPU free of other work.
#[test]
#[ignore = "exclusive: measures B1 and B2, needs an idle GPU (run alone with --run-ignored only)"]
fn ten_seconds_of_1080p60_meet_b1_and_b2_on_the_shells_device() {
    let r = run(&Config::b1b2(10.0), None).expect("run");
    budgets(&r, 10.0);
}

/// The same for 30 s.
#[test]
#[ignore = "exclusive: measures B1 and B2, needs an idle GPU (run alone with --run-ignored only)"]
fn thirty_seconds_of_1080p60_meet_b1_and_b2_on_the_shells_device() {
    let r = run(&Config::b1b2(30.0), None).expect("run");
    budgets(&r, 30.0);
}
