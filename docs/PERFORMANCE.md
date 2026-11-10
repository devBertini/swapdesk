# Performance

The numbers of the budgets in [SPEC.md](SPEC.md) as measured on the development PC. This file is written by `python scripts/measure.py`, never by hand: a number here is the output of the run described under it.

- Date: 2026-11-10
- Commit: fde6d49 with uncommitted changes
- Mode: short (30 s per configuration after a 5 s warm-up). The budgets name 10 minutes; the long mode (`--long`) runs once, when a version is hardened
- Machine:
  - Intel(R) Core(TM) Ultra 9 275HX, 24 logical processors, 31 GB of memory
  - Intel(R) Graphics (driver 32.0.101.9150)
  - NVIDIA GeForce RTX 5070 Laptop GPU (driver 32.0.16.2101)
  - Microsoft Windows 11 Home Single Language build 26300
  - Monitor of the viewer's window: \\.\DISPLAY5 at 120 Hz; viewer's device: Intel(R) Graphics

## Budgets

| ID | Budget | Measured | Verdict | Where from |
|---|---|---|---|---|
| B1 | median ≤ 40 ms, p95 ≤ 60 ms | NVENC median 6.1 ms, p95 10.4 ms; Quick Sync median 12.3 ms, p95 13.0 ms | inside | sessions at 1080p60 |
| B2 | ≥ 59 fps presented (median per 1 s window), ≤ 0.5 % lost | NVENC 60 fps, 0.00 % lost; Quick Sync 60 fps, 0.00 % lost | inside | sessions at 1080p60 |
| B3 | ≤ 120 ms median, no underrun | NVENC median 33.6 ms, 0 underruns; Quick Sync median 34.1 ms, 0 underruns | inside | sessions at 1080p60 |
| B4 | ≤ 1 s from "Aceitar" to the reverse session | 3 ms and 2 ms (one swap each way, to the grant of the reverse session) | inside | test `loopback_swap_works_both_ways_each_under_one_second` (debug build, loopback) |
| B5 | ≤ 3 s from the network coming back to the first frame | 4 ms (the link back to a resumed session, after a 10 s drop) | inside | test of `swapdesk-session` over real QUIC (debug build, loopback) |
| B6 | ≤ 1 % of total CPU for service + helpers, no session | measured in owner batch 1 | — | installed service |
| B7 | ≤ 30 MB private working set, idle | 46.9 MB | **outside** | `Swapdesk.exe --mock dispositivos`, 10 s after the start |
| B8 | ≤ 30 ms of CPU time over 30 s, idle | 0 ms | inside | the same run, the next 30 s |
| B9 | flat after warm-up: ≤ 10.0 MB per hour | NVENC 131.1 MB median, 0.25 MB from first to last sample; Quick Sync 127.2 MB median, 0.20 MB from first to last sample (the growth per hour needs the long mode) | — | the viewer process of the sessions at 1080p60 |
| B10 | host CPU in a 1080p60 session: ≤ 2.0 % of total CPU | NVENC 0.78 % of total CPU (18.6 % of one core); Quick Sync 0.88 % of total CPU (21.1 % of one core) | inside | service + helpers of the host without the synthetic source's share, sessions at 1080p60 |

B4 and B5 are the session layer's part, timed by its loopback tests: the swap up to the grant of the reverse session, and the return of the link up to the resumed session. The first frame after them and the time the viewer takes to notice that the network is back are not in these two numbers; between two machines they are measured with the cross-machine flows.

## Sessions through the whole chain

Each row is one session on this PC: the host's helper (synthetic screen, hardware encoder, sine with a marker every second), the host's service, QUIC on loopback, the viewer's service, the UI pipe, and the viewer (Media Foundation decoder and the renderer in a window).

### Latency and frames

| Configuration | Encoder | Viewer window | B1 median | p95 | p99 | max | fps median | fps min | Seconds under 59 fps | Lost |
|---|---|---|---|---|---|---|---|---|---|---|
| 1080p60, NVENC | NVIDIA NVENC H.264 on NVIDIA GeForce RTX 5070 Laptop GPU (hardware) | 1920x1080 | 6.1 ms | 10.4 ms | 11.2 ms | 591.0 ms | 60 | 26 | 1 of 30 | 0 of 1766 (0.00 %) |
| 1080p60, Quick Sync | Intel® Quick Sync Video H.264 Encoder MFT (hardware) | 1920x1080 | 12.3 ms | 13.0 ms | 16.6 ms | 51.0 ms | 60 | 49 | 1 of 30 | 0 of 1789 (0.00 %) |
| 2560x1600@60, NVENC | NVIDIA NVENC H.264 on NVIDIA GeForce RTX 5070 Laptop GPU (hardware) | 1984x1240 | 9.9 ms | 13.3 ms | 14.4 ms | 17.6 ms | 60 | 60 | 0 of 30 | 0 of 1800 (0.00 %) |
| 2560x1600@60, Quick Sync | Intel® Quick Sync Video H.264 Encoder MFT (hardware) | 1984x1240 | 13.0 ms | 21.6 ms | 31.7 ms | 110.0 ms | 60 | 46 | 1 of 30 | 0 of 1785 (0.00 %) |

"Lost" counts the frames the source handed out that never reached the screen. A second under 59 fps with nothing lost is a second in which the host handed out fewer frames: the encoder held the capture loop.

### Stages of a presented frame (median / p95, ms)

| Configuration | Encode | Transport | Queue in the viewer | Decode | Wait for the swap chain | Blit | Present |
|---|---|---|---|---|---|---|---|
| 1080p60, NVENC | 3.36 / 7.53 | 1.25 / 1.68 | 0.02 / 0.03 | 0.54 / 0.68 | 0.00 / 0.00 | 0.47 / 0.56 | 0.33 / 0.41 |
| 1080p60, Quick Sync | 9.92 / 10.33 | 0.89 / 1.24 | 0.02 / 0.03 | 0.59 / 0.74 | 0.00 / 0.00 | 0.48 / 0.57 | 0.35 / 0.43 |
| 2560x1600@60, NVENC | 6.60 / 9.12 | 1.41 / 1.92 | 0.02 / 0.03 | 0.59 / 0.80 | 0.00 / 3.00 | 0.48 / 0.63 | 0.34 / 0.49 |
| 2560x1600@60, Quick Sync | 10.16 / 15.05 | 1.10 / 4.44 | 0.02 / 0.03 | 0.70 / 0.98 | 0.00 / 1.95 | 0.47 / 0.62 | 0.33 / 0.44 |

Encode: the helper's `encode` call. Transport: from the end of the encode to the frame's arrival in the viewer process (helper pipe, host service, QUIC, viewer service, UI pipe). Queue: from the arrival to the start of the decode. The four viewer stages follow in the order shown; the stages are medians of their own and need not add up to the median of B1.

### Stream, CPU and memory

| Configuration | Stream | Frame (median) | Host service | Host helpers | Host, of total CPU | Synthetic source (taken out) | Viewer service | Viewer process | Viewer memory (median / max) |
|---|---|---|---|---|---|---|---|---|---|
| 1080p60, NVENC | 35.5 Mbps | 80.2 KB | 9.4 % | 9.2 % | 0.78 % | 16.7 % | 6.8 % | 13.4 % | 131.1 / 131.1 MB |
| 1080p60, Quick Sync | 23.1 Mbps | 45.9 KB | 6.7 % | 14.5 % | 0.88 % | 15.9 % | 6.2 % | 14.8 % | 127.2 / 127.2 MB |
| 2560x1600@60, NVENC | 50.0 Mbps | 96.8 KB | 9.1 % | 8.8 % | 0.75 % | 22.0 % | 8.4 % | 15.0 % | 179.0 / 179.7 MB |
| 2560x1600@60, Quick Sync | 36.7 Mbps | 72.1 KB | 7.7 % | 11.9 % | 0.82 % | 17.9 % | 6.4 % | 15.3 % | 172.1 / 172.1 MB |

CPU columns are percent of one core, except "of total CPU" (all 24 logical processors). The viewer process is the measuring program itself: the decoder, the renderer, the audio player, both users' UI connections and the barcode reading. The stream starts at 20 Mbps and the rate control raises it while the link is clean, so a short run averages a rising rate.

### Audio

| Configuration | B3 median | min | max | Markers heard | Underruns | Audio after video |
|---|---|---|---|---|---|---|
| 1080p60, NVENC | 33.6 ms | 33.6 ms | 43.6 ms | 30 of 30 | 0 | 27.5 ms |
| 1080p60, Quick Sync | 34.1 ms | 34.1 ms | 44.1 ms | 30 of 30 | 0 | 21.9 ms |
| 2560x1600@60, NVENC | 33.8 ms | 33.8 ms | 33.8 ms | 30 of 30 | 0 | 23.8 ms |
| 2560x1600@60, Quick Sync | 33.6 ms | 33.6 ms | 33.6 ms | 30 of 30 | 0 | 20.6 ms |

"Audio after video" is the median audio delay minus the median video latency: how long after its picture the sound of an instant is heard.

## The frame path alone

The path of one video frame from the encoder's output to the viewer's sink, in one process under a counting allocator: the helper's loop, the host's core, QUIC on loopback, the viewer's core, its UI hub and the UI connection, with OS pipes between them. No capture, codec or audio, so what is counted is the path's.

| Payload | Frames | Allocations per frame | Bytes allocated per frame | Payload-sized buffers per frame | CPU per frame |
|---|---|---|---|---|---|
| 82088 bytes at 60 fps | 600 | 184.3 | 1269238 | 15.46 | 1927 µs |

"Payload-sized buffers per frame" is the bytes allocated per frame divided by the payload size: how many buffers of the frame's size the path allocates for each frame, which is what a copy is made into. The payload is the median frame of the 1080p60 NVENC session above.

## How it is measured

- **Sources (contract C7).** The screen is the synthetic one: a moving gradient with a barcode in the top-left corner that carries the frame's counter and its time on the machine's monotonic clock. A frame is stamped with the time of its slot at the frame rate and handed to the helper's capture loop at that instant, as a texture on the adapter of the encoder under test. The sound is a 1 kHz sine with a marker burst at the start of every second. A measurement never captures the real screen, so it never films its own viewer.
- **B1.** The barcode is read back from the pixels of the swap chain's back buffer of every presented picture, and its time is compared with the clock right after `Present` returned.
- **B2.** Presented pictures per one-second window, and the counters of the source that never reached the screen.
- **B3.** The viewer's audio player (jitter buffer and Opus decoder) writes into a sink that finds each marker's onset and when it plays; the delay is that time minus the time the source produced the marker. The sound card's own buffer is not in this number: the sink plays at once.
- **B9, B10.** The private working set of the viewer process, sampled every 2 s, and the CPU time of the host's service and helpers over the measured window. The synthetic source runs inside the helper (it draws, uploads and waits for every slot, which a real capture does not): its share of the helper's processor cycles is taken out of the helper's CPU time and shown in a column of its own.
- **The viewer's window is the foreground window.** A desktop may hand the pictures of a background window to the screen at a lower rate; this PC caps them at 30 a second. A run takes the foreground for its window, as a viewer in use has it, and is refused when the window lost it or when `Present` blocks for more than half a frame time. `swapdesk-measure session --background` leaves the window in the background to show what a viewer does there.
- **Binaries.** The service is the `release-service` build and the rest the `release` build, the profiles the release is packaged from. The host's helper is the measuring program under the helper's name: the same loop and the same encoder as `swapdesk-helper`, with the synthetic sources in place of the capture and the sound card.
- **Limits.** Both machines are this PC, so the link is the loopback interface: no Wi-Fi, no second machine's clock. The decoder and the encoder share one PC's GPUs. A picture larger than the monitor is shown scaled, as the application shows it in a smaller window.

## Run it

```
python scripts/measure.py --short
```

On an idle PC with nobody at the keyboard: the viewer's window takes the foreground and a run that loses it is refused. The logs of every run stay under `target/measure`. `cargo run --release -p swapdesk-measure -- session --logs <folder>` runs one configuration by itself.
