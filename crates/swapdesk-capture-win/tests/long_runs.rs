//! Long runs, `#[ignore]`d: the 10-minute leak run (stage 12 done-when) and the capture cost over 30 s (budget B2).
//! Run one at a time: `cargo nextest run -p swapdesk-capture-win --test long_runs --run-ignored only --no-capture -E 'test(leak)'`.
//! `SWAPDESK_LEAK_SECS` shortens or lengthens the leak run (default 600). Nothing from the screen is saved or printed:
//! the numbers are sizes, counts, times and process counters.
#![cfg(windows)]

mod common;

use common::{Flicker, PerSecond, aware, counters, primary, process_cpu_ms, serial, thread_cpu_ms};
use std::time::{Duration, Instant};
use swapdesk_capture_win::{CaptureOptions, DevicePool, DxgiScreen, video_memory_in_use};
use swapdesk_platform::{CaptureConfig, ScreenSource};

const MIB: f64 = 1024.0 * 1024.0;

#[test]
#[ignore = "10 minutes: run once"]
fn leak_run_60_fps_with_periodic_reacquire() {
    aware();
    let _lock = serial();
    let secs: u64 = std::env::var("SWAPDESK_LEAK_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(600);
    // Seconds between forced re-acquires (0 = never): isolates a cost per reacquire from a cost per frame.
    let reacquire_every: u64 = std::env::var("SWAPDESK_REACQUIRE_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(30);
    let p = primary();
    let flicker = Flicker::start(&p);
    let mut screen = DxgiScreen::with_options(DevicePool::new(), CaptureOptions::default());
    screen.open(p.id, CaptureConfig { max_fps: 60 }).expect("open");
    let luid = p.adapter.0;
    println!("leak run: {secs} s, display {}x{} on adapter {luid:#x}", p.bounds.width, p.bounds.height);
    println!("t_s   frames  reacq  private_MiB  working_MiB  handles  gdi  user  vram_MiB  textures");
    let started = Instant::now();
    let cpu0 = thread_cpu_ms();
    let mut frames = 0u64;
    let mut last_stamp = 0u64;
    let mut regressions = 0u64;
    let mut per_second = PerSecond::default();
    let mut next_sample = 0u64;
    let mut next_reacquire = reacquire_every;
    let mut real_losses = 0u32;
    let mut samples: Vec<(u64, common::Counters, f64)> = Vec::new();
    while started.elapsed() < Duration::from_secs(secs) {
        let t = started.elapsed().as_secs();
        if t >= next_sample {
            let c = counters();
            let vram = video_memory_in_use(luid).unwrap_or(0) as f64 / MIB;
            println!(
                "{t:<5} {frames:<7} {:<6} {:<12.1} {:<12.1} {:<8} {:<4} {:<5} {:<9.1} {}",
                screen.stats().reacquires,
                c.private_bytes as f64 / MIB,
                c.working_set as f64 / MIB,
                c.handles,
                c.gdi_objects,
                c.user_objects,
                vram,
                screen.stats().textures_created
            );
            samples.push((t, c, vram));
            next_sample += 30;
        }
        if reacquire_every > 0 && t >= next_reacquire {
            screen.simulate_access_lost();
            assert!(screen.next_frame(Duration::from_millis(10)).is_err());
            screen.reacquire().expect("reacquire");
            next_reacquire += reacquire_every;
        }
        let next = match screen.next_frame(Duration::from_millis(500)) {
            Ok(n) => n,
            Err(e) if e.kind == swapdesk_platform::ErrorKind::AccessLost => {
                // A real loss (desktop switch, mode change, power event): recover as the helper would.
                real_losses += 1;
                println!("{t} s: real AccessLost: {e}");
                let mut tries = 0;
                while let Err(e) = screen.reacquire() {
                    tries += 1;
                    assert!(tries < 50, "reacquire kept failing: {e}");
                    std::thread::sleep(Duration::from_millis(100));
                }
                None
            }
            Err(e) => panic!("next_frame: {e}"),
        };
        if let Some(f) = next {
            let stamp = f.captured_at.0;
            if stamp < last_stamp {
                regressions += 1;
            }
            last_stamp = stamp;
            frames += 1;
            per_second.add(started.elapsed());
        }
    }
    let elapsed = started.elapsed();
    let c = counters();
    let vram = video_memory_in_use(luid).unwrap_or(0) as f64 / MIB;
    samples.push((elapsed.as_secs(), c, vram));
    let cpu = thread_cpu_ms() - cpu0;
    let s = screen.stats();
    println!(
        "done: {frames} frames in {elapsed:?} = {:.2} fps; capture thread CPU {cpu:.0} ms ({:.2}% of one core); timestamp regressions {regressions}; real AccessLost events {real_losses}; {s:?}",
        frames as f64 / elapsed.as_secs_f64(),
        cpu / elapsed.as_secs_f64() / 10.0
    );
    // Warm-up is the first sample after 60 s; growth is measured from there to the end.
    let base = samples.iter().find(|(t, ..)| *t >= 60).or(samples.first()).copied().expect("samples");
    let end = *samples.last().expect("samples");
    let d_private = (end.1.private_bytes as f64 - base.1.private_bytes as f64) / MIB;
    let d_vram = end.2 - base.2;
    let paints = flicker.paints.load(std::sync::atomic::Ordering::Relaxed);
    println!(
        "the test's own flicker window repainted {paints} times ({:.0}/s on a {} Hz display)",
        paints as f64 / elapsed.as_secs_f64(),
        p.refresh_mhz / 1000
    );
    println!(
        "B2 windows: median {} fps per 1 s window, minimum {}, {} of {} windows under 59 fps",
        per_second.median(),
        per_second.min(),
        per_second.windows_below(59),
        per_second.windows()
    );
    println!(
        "growth since t={}s: private {d_private:+.2} MiB, handles {:+}, gdi {:+}, user {:+}, vram {d_vram:+.2} MiB",
        base.0,
        i64::from(end.1.handles) - i64::from(base.1.handles),
        i64::from(end.1.gdi_objects) - i64::from(base.1.gdi_objects),
        i64::from(end.1.user_objects) - i64::from(base.1.user_objects)
    );
    assert_eq!(regressions, 0);
    assert!(per_second.median() >= 59.0, "median {} fps per window", per_second.median());
    assert!(d_private < 8.0, "private bytes grew {d_private:.2} MiB");
    // Handles and USER objects wobble by one or two with the desktop (DWM, the test window's own messages): the first
    // run of 592 s showed handles 229..233 and user objects 8..9 with no trend. A leak per reacquire (19 of them)
    // or per frame (35 000) would show far beyond these margins.
    let max_handles = samples.iter().filter(|(t, ..)| *t >= 60).map(|(_, c, _)| c.handles).max().unwrap_or(0);
    let max_user = samples.iter().filter(|(t, ..)| *t >= 60).map(|(_, c, _)| c.user_objects).max().unwrap_or(0);
    assert!(
        end.1.handles <= base.1.handles + 4 && max_handles <= base.1.handles + 6,
        "handles {} -> {} (max {max_handles})",
        base.1.handles,
        end.1.handles
    );
    assert!(end.1.gdi_objects <= base.1.gdi_objects + 1, "gdi objects grew");
    assert!(
        end.1.user_objects <= base.1.user_objects + 2 && max_user <= base.1.user_objects + 3,
        "user objects {} -> {} (max {max_user})",
        base.1.user_objects,
        end.1.user_objects
    );
    assert!(d_vram < 16.0, "video memory grew {d_vram:.2} MiB");
    assert!(s.textures_created <= 8, "the texture pool kept allocating: {}", s.textures_created);
}

#[test]
#[ignore = "30 s of measurement"]
fn capture_cost_over_30_seconds() {
    aware();
    let _lock = serial();
    let p = primary();
    let flicker = Flicker::start(&p);
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    for (label, fps, secs) in [("60 fps cap, native mode", 60u16, 30u64), ("uncapped, native mode", 0u16, 10u64)] {
        let mut screen = DxgiScreen::new();
        screen.open(p.id, CaptureConfig { max_fps: fps }).expect("open");
        // Warm up: the first frames allocate the pool and settle the clocks.
        for _ in 0..30 {
            let _ = screen.next_frame(Duration::from_millis(500)).expect("next_frame");
        }
        let (t0, c_thread, c_proc) = (Instant::now(), thread_cpu_ms(), process_cpu_ms());
        let s0 = screen.stats();
        let mut frames = 0u64;
        let mut worst = Duration::ZERO;
        let mut gaps_over_50ms = 0u32;
        let mut per_second = PerSecond::default();
        let mut last_frame = Instant::now();
        let paints0 = flicker.paints.load(std::sync::atomic::Ordering::Relaxed);
        while t0.elapsed() < Duration::from_secs(secs) {
            let call = Instant::now();
            if let Some(f) = screen.next_frame(Duration::from_millis(500)).expect("next_frame") {
                frames += 1;
                per_second.add(t0.elapsed());
                drop(f);
                if last_frame.elapsed() > Duration::from_millis(50) {
                    gaps_over_50ms += 1;
                }
                last_frame = Instant::now();
            }
            worst = worst.max(call.elapsed());
        }
        let wall = t0.elapsed().as_secs_f64();
        let thread = thread_cpu_ms() - c_thread;
        let process = process_cpu_ms() - c_proc;
        let s = screen.stats();
        let paints = flicker.paints.load(std::sync::atomic::Ordering::Relaxed) - paints0;
        println!(
            "{label}: median {} fps per 1 s window, minimum {}, {} of {} windows under 59 fps",
            per_second.median(),
            per_second.min(),
            per_second.windows_below(59),
            per_second.windows()
        );
        println!(
            "{label}: frame gaps over 50 ms: {gaps_over_50ms}; the flicker window repainted {paints} times ({:.0}/s)",
            paints as f64 / wall
        );
        println!(
            "{label}: {}x{} on {} cores: {frames} frames in {wall:.1} s = {:.1} fps; capture thread CPU {thread:.0} ms = {:.2}% of one core = {:.3}% of all cores, {:.2} ms per frame; process CPU (with the test's flicker window) {process:.0} ms; coalesced {}; timeouts {}; longest next_frame {worst:?}",
            p.bounds.width,
            p.bounds.height,
            cores,
            frames as f64 / wall,
            thread / wall / 10.0,
            thread / wall / 10.0 / cores as f64,
            thread / frames.max(1) as f64,
            s.coalesced - s0.coalesced,
            s.timeouts - s0.timeouts
        );
        if fps == 60 {
            assert!(per_second.median() >= 59.0, "B2: median {} fps per window", per_second.median());
        }
    }
}
