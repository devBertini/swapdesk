#!/usr/bin/env python
"""Measures the budgets of docs/SPEC.md on this PC and writes docs/PERFORMANCE.md.

Standard library only. Run it on an idle PC (no build, no other session, nobody at the keyboard: each session's
viewer window takes the foreground and must keep it):

    python scripts/measure.py --short     # 30 s per configuration: the proof of a stage
    python scripts/measure.py --long      # 10 min per configuration: once, when a version is hardened

Options: --skip-build (the binaries are already built from this tree), --only <name>[,<name>] (configurations by
their short name; the document is then not rewritten), --strict (exit 1 when a judged budget is outside),
--logs <folder> (default target/measure).

What runs, in this order:

1. builds: the service in its shipping profile, `swapdesk-measure` and `Swapdesk.exe` in release, and the two test
   binaries of step 5 (so nothing compiles while something is measured);
2. `swapdesk-measure session` for 1080p60 and 2560x1600@60 on NVENC and on Quick Sync: the whole chain (helper,
   service, QUIC on loopback, service, viewer) on the synthetic sources. B1, B2, B3, B9, B10, the stages of a frame,
   the audio/video offset;
3. `swapdesk-measure path`: allocations, allocated bytes and CPU time per frame on the frame path alone;
4. `swapdesk-measure idle` on `Swapdesk.exe --mock dispositivos`: B7 and B8;
5. the loopback tests that time a swap (B4) and a reconnect (B5), for the times they print.

B6 (idle host CPU) belongs to the installed service and is measured by hand; the document says so.
Exit code: 0 every run measured, 1 a run failed (the document is left as it was) or, with --strict, a budget is outside.
"""
import argparse
import datetime
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUTPUT = os.path.join(ROOT, "docs", "PERFORMANCE.md")

# (short name, title, size, encoder). The budgets B1 to B3 and B10 are written for 1080p60.
CONFIGS = (
    ("1080p-nvenc", "1080p60, NVENC", "1920x1080", "nvenc"),
    ("1080p-quicksync", "1080p60, Quick Sync", "1920x1080", "quicksync"),
    ("1600p-nvenc", "2560x1600@60, NVENC", "2560x1600", "nvenc"),
    ("1600p-quicksync", "2560x1600@60, Quick Sync", "2560x1600", "quicksync"),
)
BUDGET_CONFIGS = ("1080p-nvenc", "1080p-quicksync")

# The numbers of docs/SPEC.md "Budgets". B9 and B10 were set from the first end-to-end measurement.
B1_MEDIAN_MS, B1_P95_MS = 40.0, 60.0
B2_FPS, B2_LOST_PERCENT = 59.0, 0.5
B3_MEDIAN_MS = 120.0
B4_MS = 1000.0
B5_MS = 3000.0
B7_MB = 30.0
B8_MS = 30.0
B9_MB_PER_HOUR = 10.0
B10_TOTAL_PERCENT = 2.0

B4_TEST = ("swapdesk-swap", "swap", "loopback_swap_works_both_ways_each_under_one_second")
B5_TEST = (
    "swapdesk-session",
    "resume",
    "over_real_quic_a_dead_link_is_a_lost_connection_and_the_viewer_resumes_within_3_seconds_of_the_link_coming_back",
)


def run(cmd, log=None, check=True):
    """Runs `cmd` from the repository root; its output goes to `log` when given. Returns (exit code, output)."""
    done = subprocess.run(
        cmd, cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, encoding="utf-8", errors="replace"
    )
    if log:
        with open(log, "w", encoding="utf-8", newline="\n") as f:
            f.write(done.stdout)
    if check and done.returncode != 0:
        sys.stderr.write(done.stdout[-4000:])
        raise SystemExit(f"measure: `{' '.join(cmd)}` exited with {done.returncode}")
    return done.returncode, done.stdout


def parse_fields(text):
    """`name=value` lines as a dict; a value that is a number becomes a float."""
    out = {}
    for line in text.splitlines():
        name, sep, value = line.partition("=")
        if not sep or not re.fullmatch(r"[a-z0-9_]+", name):
            continue
        try:
            out[name] = float(value)
        except ValueError:
            out[name] = value
    return out


def num(value, digits=1):
    """A number for the document; a missing or non-finite one is a dash."""
    if not isinstance(value, float) or value != value:
        return "—"
    return f"{value:.{digits}f}"


def pair(fields, name, digits=1):
    """`median / p95` of a series."""
    return f"{num(fields.get(name + '_median'), digits)} / {num(fields.get(name + '_p95'), digits)}"


def verdict(ok):
    return "inside" if ok else "**outside**"


def powershell(command):
    code, out = run(["powershell", "-NoProfile", "-Command", command], check=False)
    return out.strip() if code == 0 else ""


def machine():
    """The hardware and the system, with no name of the machine or of its user."""
    cpu = powershell("(Get-CimInstance Win32_Processor | Select-Object -First 1).Name")
    cores = powershell("(Get-CimInstance Win32_Processor | Measure-Object NumberOfLogicalProcessors -Sum).Sum")
    ram = powershell("[math]::Round((Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory / 1GB)")
    gpus = powershell(
        "Get-CimInstance Win32_VideoController | ForEach-Object { $_.Name + ' (driver ' + $_.DriverVersion + ')' }"
    )
    system = powershell("$o = Get-CimInstance Win32_OperatingSystem; $o.Caption + ' build ' + $o.BuildNumber")
    lines = []
    if cpu:
        lines.append(f"{' '.join(cpu.split())}, {cores} logical processors, {ram} GB of memory")
    for gpu in [g.strip() for g in gpus.splitlines() if g.strip()]:
        lines.append(gpu)
    if system:
        lines.append(" ".join(system.split()))
    return lines


def commit():
    _, head = run(["git", "rev-parse", "--short", "HEAD"], check=False)
    _, dirty = run(["git", "status", "--porcelain", "--untracked-files=no"], check=False)
    return head.strip() + (" with uncommitted changes" if dirty.strip() else "")


def build(logs):
    cargo = [
        ["cargo", "build", "--profile", "release-service", "--locked", "-p", "swapdesk-service"],
        ["cargo", "build", "--release", "--locked", "-p", "swapdesk-measure"],
        ["cargo", "build", "--release", "--locked", "-p", "swapdesk-app"],
        ["cargo", "test", "--locked", "--no-run", "-p", B4_TEST[0], "--test", B4_TEST[1]],
        ["cargo", "test", "--locked", "--no-run", "-p", B5_TEST[0], "--test", B5_TEST[1]],
    ]
    for i, cmd in enumerate(cargo):
        print(f"measure: {' '.join(cmd)}", flush=True)
        run(cmd, log=os.path.join(logs, f"build-{i}.log"))


def measure_tool():
    exe = os.path.join(ROOT, "target", "release", "swapdesk-measure.exe")
    if not os.path.exists(exe):
        raise SystemExit(f"measure: {os.path.relpath(exe, ROOT)} is missing (run without --skip-build)")
    return exe


def tool(args, logs, name):
    """One run of swapdesk-measure: its fields, or None when it failed."""
    folder = os.path.join(logs, name)
    os.makedirs(folder, exist_ok=True)
    own = ["--out", os.path.join(folder, "result.txt")]
    if args[0] == "session":
        own += ["--logs", folder]
    # What follows `--` belongs to the program an `idle` run starts.
    split = args.index("--") if "--" in args else len(args)
    cmd = [measure_tool()] + args[:split] + own + args[split:]
    print(f"measure: {name}", flush=True)
    code, out = run(cmd, log=os.path.join(folder, "run.log"), check=False)
    if code != 0:
        sys.stderr.write(out[-3000:])
        print(f"measure: {name} failed with exit code {code} (log in {os.path.relpath(folder, ROOT)})", flush=True)
        return None
    return parse_fields(out)


def timed_test(test, pattern, logs, name):
    """Runs one loopback test and returns the times (ms) it prints, or None when it failed."""
    package, target, case = test
    cmd = ["cargo", "test", "--locked", "-p", package, "--test", target, case, "--", "--exact", "--nocapture"]
    print(f"measure: {name}", flush=True)
    code, out = run(cmd, log=os.path.join(logs, f"{name}.log"), check=False)
    times = [float(m) for m in re.findall(pattern, out)]
    if code != 0 or not times:
        sys.stderr.write(out[-2000:])
        return None
    return times


def budgets_table(results, path, idle, b4, b5, long_mode):
    """The rows of the budgets table and whether every judged budget is inside."""
    judged = [results[name] for name in BUDGET_CONFIGS if name in results]
    labels = {"1080p-nvenc": "NVENC", "1080p-quicksync": "Quick Sync"}

    def each(render):
        return "; ".join(f"{labels[n]} {render(results[n])}" for n in BUDGET_CONFIGS if n in results)

    rows, inside = [], True

    def row(ident, budget, measured, ok, source):
        nonlocal inside
        if ok is not None:
            inside = inside and ok
        rows.append(f"| {ident} | {budget} | {measured} | {'—' if ok is None else verdict(ok)} | {source} |")

    row(
        "B1",
        f"median ≤ {B1_MEDIAN_MS:.0f} ms, p95 ≤ {B1_P95_MS:.0f} ms",
        each(lambda f: f"median {num(f.get('b1_ms_median'))} ms, p95 {num(f.get('b1_ms_p95'))} ms"),
        all(f["b1_ms_median"] <= B1_MEDIAN_MS and f["b1_ms_p95"] <= B1_P95_MS for f in judged),
        "sessions at 1080p60",
    )
    row(
        "B2",
        f"≥ {B2_FPS:.0f} fps presented (median per 1 s window), ≤ {B2_LOST_PERCENT} % lost",
        each(lambda f: f"{num(f.get('b2_fps_median'), 0)} fps, {num(f.get('b2_lost_percent'), 2)} % lost"),
        all(f["b2_fps_median"] >= B2_FPS and f["b2_lost_percent"] <= B2_LOST_PERCENT for f in judged),
        "sessions at 1080p60",
    )
    row(
        "B3",
        f"≤ {B3_MEDIAN_MS:.0f} ms median, no underrun",
        each(
            lambda f: f"median {num(f.get('b3_ms_median'))} ms, "
            f"{num(f.get('b3_underruns', 0.0) + f.get('b3_sink_gaps', 0.0), 0)} underruns"
        ),
        all(
            f["b3_ms_median"] <= B3_MEDIAN_MS and f["b3_underruns"] + f["b3_sink_gaps"] == 0
            for f in judged
        ),
        "sessions at 1080p60",
    )
    row(
        "B4",
        f"≤ {B4_MS / 1000:.0f} s from \"Aceitar\" to the reverse session",
        " and ".join(f"{t:.0f} ms" for t in b4) + " (one swap each way, to the grant of the reverse session)",
        all(t <= B4_MS for t in b4),
        f"test `{B4_TEST[2]}` (debug build, loopback)",
    )
    row(
        "B5",
        f"≤ {B5_MS / 1000:.0f} s from the network coming back to the first frame",
        f"{b5[0]:.0f} ms (the link back to a resumed session, after a 10 s drop)",
        b5[0] <= B5_MS,
        "test of `swapdesk-session` over real QUIC (debug build, loopback)",
    )
    row("B6", "≤ 1 % of total CPU for service + helpers, no session", "measured in owner batch 1", None, "installed service")
    row(
        "B7",
        f"≤ {B7_MB:.0f} MB private working set, idle",
        f"{num(idle.get('b7_private_mb'))} MB",
        idle["b7_private_mb"] <= B7_MB,
        "`Swapdesk.exe --mock dispositivos`, 10 s after the start",
    )
    row(
        "B8",
        f"≤ {B8_MS:.0f} ms of CPU time over 30 s, idle",
        f"{num(idle.get('b8_cpu_ms'), 0)} ms",
        idle["b8_cpu_ms"] <= B8_MS,
        "the same run, the next 30 s",
    )
    growth = each(
        lambda f: f"{num(f.get('b9_median_mb'))} MB median, "
        f"{num(f.get('b9_last_mb', float('nan')) - f.get('b9_first_mb', float('nan')), 2)} MB from first to last sample"
        + (f", {num(f.get('b9_slope_mb_per_hour'))} MB/h" if long_mode else "")
    )
    b9_ok = None
    if long_mode and B9_MB_PER_HOUR is not None:
        b9_ok = all(f["b9_slope_mb_per_hour"] <= B9_MB_PER_HOUR for f in judged)
    row(
        "B9",
        "flat after warm-up" + (f": ≤ {B9_MB_PER_HOUR} MB per hour" if B9_MB_PER_HOUR is not None else ""),
        growth + ("" if long_mode else " (the growth per hour needs the long mode)"),
        b9_ok,
        "the viewer process of the sessions at 1080p60",
    )
    b10_ok = None
    if B10_TOTAL_PERCENT is not None:
        b10_ok = all(f["b10_host_total_pct"] <= B10_TOTAL_PERCENT for f in judged)
    row(
        "B10",
        "host CPU in a 1080p60 session"
        + (f": ≤ {B10_TOTAL_PERCENT} % of total CPU" if B10_TOTAL_PERCENT is not None else ""),
        each(
            lambda f: f"{num(f.get('b10_host_total_pct'), 2)} % of total CPU "
            f"({num(f.get('b10_host_core_pct'))} % of one core)"
        ),
        b10_ok,
        "service + helpers of the host without the synthetic source's share, sessions at 1080p60",
    )
    return rows, inside


def document(mode, seconds, results, path, idle, b4, b5):
    long_mode = mode == "long"
    rows, inside = budgets_table(results, path, idle, b4, b5, long_mode)
    done = [(name, title) for name, title, _, _ in CONFIGS if name in results]
    first = results[done[0][0]]
    out = []
    add = out.append
    add("# Performance")
    add("")
    add(
        "The numbers of the budgets in [SPEC.md](SPEC.md) as measured on the development PC. This file is written by "
        "`python scripts/measure.py`, never by hand: a number here is the output of the run described under it."
    )
    add("")
    add(f"- Date: {datetime.date.today().isoformat()}")
    add(f"- Commit: {commit()}")
    if long_mode:
        add(f"- Mode: long ({seconds} s per configuration after a {first.get('warmup_s', 5.0):.0f} s warm-up)")
    else:
        add(
            f"- Mode: short ({seconds} s per configuration after a {first.get('warmup_s', 5.0):.0f} s warm-up). "
            "The budgets name 10 minutes; the long mode (`--long`) runs once, when a version is hardened"
        )
    add("- Machine:")
    for line in machine():
        add(f"  - {line}")
    add(f"  - Monitor of the viewer's window: {first.get('monitor', 'unknown')}; viewer's device: {first.get('viewer_adapter', 'unknown')}")
    add("")
    add("## Budgets")
    add("")
    add("| ID | Budget | Measured | Verdict | Where from |")
    add("|---|---|---|---|---|")
    out.extend(rows)
    add("")
    add(
        "B4 and B5 are the session layer's part, timed by its loopback tests: the swap up to the grant of the "
        "reverse session, and the return of the link up to the resumed session. The first frame after them and the "
        "time the viewer takes to notice that the network is back are not in these two numbers; between two "
        "machines they are measured with the cross-machine flows."
    )
    add("")
    add("## Sessions through the whole chain")
    add("")
    add(
        "Each row is one session on this PC: the host's helper (synthetic screen, hardware encoder, sine with a "
        "marker every second), the host's service, QUIC on loopback, the viewer's service, the UI pipe, and the "
        "viewer (Media Foundation decoder and the renderer in a window)."
    )
    add("")
    add("### Latency and frames")
    add("")
    add(
        "| Configuration | Encoder | Viewer window | B1 median | p95 | p99 | max | fps median | fps min | "
        "Seconds under 59 fps | Lost |"
    )
    add("|---|---|---|---|---|---|---|---|---|---|---|")
    for name, title in done:
        f = results[name]
        add(
            f"| {title} | {f.get('encoder', 'unknown')} | {f.get('window', 'unknown')} | "
            f"{num(f.get('b1_ms_median'))} ms | "
            f"{num(f.get('b1_ms_p95'))} ms | {num(f.get('b1_ms_p99'))} ms | {num(f.get('b1_ms_max'))} ms | "
            f"{num(f.get('b2_fps_median'), 0)} | {num(f.get('b2_fps_min'), 0)} | "
            f"{num(f.get('b2_windows_under_59'), 0)} of {num(f.get('b2_windows'), 0)} | "
            f"{num(f.get('frames_lost'), 0)} of {num(f.get('frames_source'), 0)} "
            f"({num(f.get('b2_lost_percent'), 2)} %) |"
        )
    add("")
    add(
        "\"Lost\" counts the frames the source handed out that never reached the screen. A second under 59 fps with "
        "nothing lost is a second in which the host handed out fewer frames: the encoder held the capture loop."
    )
    add("")
    add("### Stages of a presented frame (median / p95, ms)")
    add("")
    add("| Configuration | Encode | Transport | Queue in the viewer | Decode | Wait for the swap chain | Blit | Present |")
    add("|---|---|---|---|---|---|---|---|")
    for name, title in done:
        f = results[name]
        add(
            f"| {title} | {pair(f, 'stage_encode_ms', 2)} | {pair(f, 'stage_transport_ms', 2)} | "
            f"{pair(f, 'stage_queue_ms', 2)} | {pair(f, 'stage_decode_ms', 2)} | {pair(f, 'stage_wait_ms', 2)} | "
            f"{pair(f, 'stage_blit_ms', 2)} | {pair(f, 'stage_present_ms', 2)} |"
        )
    add("")
    add(
        "Encode: the helper's `encode` call. Transport: from the end of the encode to the frame's arrival in the "
        "viewer process (helper pipe, host service, QUIC, viewer service, UI pipe). Queue: from the arrival to the "
        "start of the decode. The four viewer stages follow in the order shown; the stages are medians of their own "
        "and need not add up to the median of B1."
    )
    add("")
    add("### Stream, CPU and memory")
    add("")
    add(
        "| Configuration | Stream | Frame (median) | Host service | Host helpers | Host, of total CPU | "
        "Synthetic source (taken out) | Viewer service | Viewer process | Viewer memory (median / max) |"
    )
    add("|---|---|---|---|---|---|---|---|---|---|")
    for name, title in done:
        f = results[name]
        add(
            f"| {title} | {num(f.get('stream_mbps'))} Mbps | {num(f.get('stream_frame_kb_median'))} KB | "
            f"{num(f.get('cpu_host_service_core_pct'))} % | {num(f.get('cpu_host_helpers_core_pct'))} % | "
            f"{num(f.get('b10_host_total_pct'), 2)} % | {num(f.get('cpu_host_source_core_pct'))} % | "
            f"{num(f.get('cpu_viewer_service_core_pct'))} % | {num(f.get('cpu_viewer_core_pct'))} % | "
            f"{num(f.get('b9_median_mb'))} / {num(f.get('b9_max_mb'))} MB |"
        )
    add("")
    add(
        f"CPU columns are percent of one core, except \"of total CPU\" (all {num(first.get('logical_processors'), 0)} "
        "logical processors). The viewer process is the measuring program itself: the decoder, the renderer, the "
        "audio player, both users' UI connections and the barcode reading. The stream starts at 20 Mbps and the rate "
        "control raises it while the link is clean, so a short run averages a rising rate."
    )
    add("")
    add("### Audio")
    add("")
    add("| Configuration | B3 median | min | max | Markers heard | Underruns | Audio after video |")
    add("|---|---|---|---|---|---|---|")
    for name, title in done:
        f = results[name]
        add(
            f"| {title} | {num(f.get('b3_ms_median'))} ms | {num(f.get('b3_ms_min'))} ms | "
            f"{num(f.get('b3_ms_max'))} ms | {num(f.get('b3_onsets'), 0)} of {num(f.get('b3_markers'), 0)} | "
            f"{num(f.get('b3_underruns', 0.0) + f.get('b3_sink_gaps', 0.0), 0)} | {num(f.get('av_offset_ms'))} ms |"
        )
    add("")
    add(
        "\"Audio after video\" is the median audio delay minus the median video latency: how long after its picture "
        "the sound of an instant is heard."
    )
    add("")
    add("## The frame path alone")
    add("")
    add(
        "The path of one video frame from the encoder's output to the viewer's sink, in one process under a counting "
        "allocator: the helper's loop, the host's core, QUIC on loopback, the viewer's core, its UI hub and the UI "
        "connection, with OS pipes between them. No capture, codec or audio, so what is counted is the path's."
    )
    add("")
    add("| Payload | Frames | Allocations per frame | Bytes allocated per frame | Payload-sized buffers per frame | CPU per frame |")
    add("|---|---|---|---|---|---|")
    add(
        f"| {num(path.get('path_payload_bytes'), 0)} bytes at {num(path.get('path_fps'), 0)} fps | "
        f"{num(path.get('path_frames'), 0)} | {num(path.get('path_allocations_per_frame'))} | "
        f"{num(path.get('path_allocated_bytes_per_frame'), 0)} | {num(path.get('path_payload_copies_per_frame'), 2)} | "
        f"{num(path.get('path_cpu_us_per_frame'), 0)} µs |"
    )
    add("")
    add(
        "\"Payload-sized buffers per frame\" is the bytes allocated per frame divided by the payload size: how many "
        "buffers of the frame's size the path allocates for each frame, which is what a copy is made into. The "
        "payload is the median frame of the 1080p60 NVENC session above."
    )
    add("")
    add("## How it is measured")
    add("")
    add(
        "- **Sources (contract C7).** The screen is the synthetic one: a moving gradient with a barcode in the "
        "top-left corner that carries the frame's counter and its time on the machine's monotonic clock. A frame is "
        "stamped with the time of its slot at the frame rate and handed to the helper's capture loop at that "
        "instant, as a texture on the adapter of the encoder under test. The sound is a 1 kHz sine with a marker "
        "burst at the start of every second. A measurement never captures the real screen, so it never films its "
        "own viewer."
    )
    add(
        "- **B1.** The barcode is read back from the pixels of the swap chain's back buffer of every presented "
        "picture, and its time is compared with the clock right after `Present` returned."
    )
    add(
        "- **B2.** Presented pictures per one-second window, and the counters of the source that never reached "
        "the screen."
    )
    add(
        "- **B3.** The viewer's audio player (jitter buffer and Opus decoder) writes into a sink that finds each "
        "marker's onset and when it plays; the delay is that time minus the time the source produced the marker. "
        "The sound card's own buffer is not in this number: the sink plays at once."
    )
    add(
        "- **B9, B10.** The private working set of the viewer process, sampled every 2 s, and the CPU time of the "
        "host's service and helpers over the measured window. The synthetic source runs inside the helper (it "
        "draws, uploads and waits for every slot, which a real capture does not): its share of the helper's "
        "processor cycles is taken out of the helper's CPU time and shown in a column of its own."
    )
    add(
        "- **The viewer's window is the foreground window.** A desktop may hand the pictures of a background "
        "window to the screen at a lower rate; this PC caps them at 30 a second. A run takes the foreground for "
        "its window, as a viewer in use has it, and is refused when the window lost it or when `Present` blocks "
        "for more than half a frame time. `swapdesk-measure session --background` leaves the window in the "
        "background to show what a viewer does there."
    )
    add(
        "- **Binaries.** The service is the `release-service` build and the rest the `release` build, the profiles "
        "the release is packaged from. The host's helper is the measuring program under the helper's name: the "
        "same loop and the same encoder as `swapdesk-helper`, with the synthetic sources in place of the capture "
        "and the sound card."
    )
    add(
        "- **Limits.** Both machines are this PC, so the link is the loopback interface: no Wi-Fi, no second "
        "machine's clock. The decoder and the encoder share one PC's GPUs. A picture larger than the monitor is "
        "shown scaled, as the application shows it in a smaller window."
    )
    add("")
    add("## Run it")
    add("")
    add("```")
    add("python scripts/measure.py --short")
    add("```")
    add("")
    add(
        "On an idle PC with nobody at the keyboard: the viewer's window takes the foreground and a run that loses it "
        "is refused. The logs of every run stay under `target/measure`. "
        "`cargo run --release -p swapdesk-measure -- session --logs <folder>` runs one configuration by itself."
    )
    add("")
    return "\n".join(out), inside


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--short", action="store_true")
    mode.add_argument("--long", action="store_true")
    parser.add_argument("--skip-build", action="store_true")
    parser.add_argument("--only")
    parser.add_argument("--strict", action="store_true")
    parser.add_argument("--logs", default=os.path.join(ROOT, "target", "measure"))
    args = parser.parse_args()

    seconds, path_seconds = (600, 60) if args.long else (30, 10)
    logs = os.path.abspath(args.logs)
    os.makedirs(logs, exist_ok=True)
    if not args.skip_build:
        build(logs)
    wanted = set(args.only.split(",")) if args.only else None
    unknown = (wanted or set()) - {c[0] for c in CONFIGS}
    if unknown:
        raise SystemExit(f"measure: unknown configuration {', '.join(sorted(unknown))}")

    failed = False
    results = {}
    for name, _title, size, encoder in CONFIGS:
        if wanted and name not in wanted:
            continue
        fields = tool(["session", "--size", size, "--fps", "60", "--encoder", encoder, "--seconds", str(seconds)], logs, name)
        if fields is None:
            failed = True
            continue
        results[name] = fields
        print(
            f"measure: {name}: B1 median {num(fields.get('b1_ms_median'))} ms, p95 {num(fields.get('b1_ms_p95'))} ms; "
            f"{num(fields.get('b2_fps_median'), 0)} fps, {num(fields.get('b2_lost_percent'), 2)} % lost; "
            f"B3 median {num(fields.get('b3_ms_median'))} ms",
            flush=True,
        )
    if wanted:
        return 1 if failed else 0

    reference = results.get("1080p-nvenc", {}).get("stream_frame_kb_median")
    payload = int(reference * 1024) if isinstance(reference, float) and reference == reference and reference > 0 else 40000
    path = tool(["path", "--seconds", str(path_seconds), "--payload", str(payload), "--fps", "60"], logs, "path")
    app = os.path.join(ROOT, "target", "release", "Swapdesk.exe")
    idle = tool(["idle", "--exe", app, "--", "--mock", "dispositivos"], logs, "idle")
    b4 = timed_test(B4_TEST, r"B4: swap \d[^\n]*? in ([0-9.]+) ms", logs, "b4-swap")
    b5 = timed_test(B5_TEST, r"B5 \(session part\)[^\n]*? in ([0-9.]+) ms", logs, "b5-reconnect")
    if failed or None in (path, idle, b4, b5) or any(name not in results for name in BUDGET_CONFIGS):
        print("measure: a run failed; docs/PERFORMANCE.md is left as it was", flush=True)
        return 1

    text, inside = document("long" if args.long else "short", seconds, results, path, idle, b4, b5)
    with open(OUTPUT, "w", encoding="utf-8", newline="\n") as f:
        f.write(text)
    print(f"measure: wrote {os.path.relpath(OUTPUT, ROOT)}; every judged budget inside: {inside}", flush=True)
    return 1 if args.strict and not inside else 0


if __name__ == "__main__":
    sys.exit(main())
