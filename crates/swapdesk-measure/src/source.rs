//! The measurement's sources (contract C7), in the form the host's helper gets them from the real machine.
//!
//! [`TimedScreen`]: the synthetic screen as GPU textures on the adapter the encoder under test lives on, one frame per
//! slot of the frame rate. A frame is prepared before its slot (barcode drawn with the slot's time on the machine's
//! monotonic clock, texture uploaded) and handed out at the slot, so the upload, which the real capture does not have,
//! stays outside the latency. A frame that could not be ready by its slot is handed out late with the slot's stamp:
//! the lateness then counts as latency, never the other way.
//!
//! [`marked_sine`]: `SineSource` on the same clock, with its marker log appended to a file for the measuring process.

use std::cell::RefCell;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use swapdesk_capture_win::QpcClock;
use swapdesk_codec_win::{TextureUploader, VENDOR_NVIDIA, list_adapters};
use swapdesk_fakes::{FakeClock, SineSource, SyntheticScreen, draw_barcode};
use swapdesk_platform::{
    AdapterId, CaptureConfig, Clock, CursorUpdate, DisplayId, DisplayInfo, ErrorKind, Frame, MonoMicros, Pixels,
    PlatformError, Rect, Result, ScreenSource,
};
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentThread};
use windows::Win32::System::WindowsProgramming::{QueryProcessCycleTime, QueryThreadCycleTime};

/// PCI vendor id of Intel (Quick Sync).
const VENDOR_INTEL: u32 = 0x8086;
/// Distinct pictures the screen cycles through (each is a full BGRA frame in memory).
const POOL: usize = 4;
/// The end of the wait for a slot is spun, not slept: the timer's resolution is a millisecond.
const SPIN: Duration = Duration::from_micros(1500);

/// The encoder family a run measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// NVENC on the NVIDIA adapter.
    Nvenc,
    /// The Media Foundation hardware encoder of the Intel adapter (Quick Sync).
    QuickSync,
}

impl Family {
    pub fn name(self) -> &'static str {
        match self {
            Family::Nvenc => "nvenc",
            Family::QuickSync => "quicksync",
        }
    }

    pub fn parse(s: &str) -> Option<Family> {
        match s {
            "nvenc" => Some(Family::Nvenc),
            "quicksync" => Some(Family::QuickSync),
            _ => None,
        }
    }

    fn vendor(self) -> u32 {
        match self {
            Family::Nvenc => VENDOR_NVIDIA,
            Family::QuickSync => VENDOR_INTEL,
        }
    }
}

/// What the helper stand-in is told through the environment (`SWAPDESK_MEASURE`): `<w>x<h>@<fps>,<family>,<markers>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSpec {
    pub width: u32,
    pub height: u32,
    pub fps: u16,
    pub family: Family,
    /// Where the audio source appends `seq captured_at_us` for every marker it plays.
    pub markers: PathBuf,
}

pub const SPEC_VAR: &str = "SWAPDESK_MEASURE";

impl SourceSpec {
    pub fn to_env(&self) -> String {
        format!("{}x{}@{},{},{}", self.width, self.height, self.fps, self.family.name(), self.markers.display())
    }

    pub fn parse(text: &str) -> Option<SourceSpec> {
        let mut parts = text.splitn(3, ',');
        let (mode, family, markers) = (parts.next()?, parts.next()?, parts.next()?);
        let (size, fps) = mode.split_once('@')?;
        let (w, h) = size.split_once('x')?;
        Some(SourceSpec {
            width: w.parse().ok()?,
            height: h.parse().ok()?,
            fps: fps.parse().ok()?,
            family: Family::parse(family)?,
            markers: PathBuf::from(markers),
        })
    }

    pub fn from_env() -> Option<SourceSpec> {
        Self::parse(&std::env::var(SPEC_VAR).ok()?)
    }
}

fn err(kind: ErrorKind, op: &'static str, detail: impl Into<String>) -> PlatformError {
    PlatformError { kind, op, os_code: None, detail: detail.into() }
}

/// The hardware adapter of `family` on this machine.
pub fn adapter_of(family: Family) -> Result<(AdapterId, String)> {
    list_adapters()?
        .into_iter()
        .find(|a| !a.software && a.vendor_id == family.vendor())
        .map(|a| (a.id, a.name))
        .ok_or_else(|| err(ErrorKind::Unavailable, "adapter_of", format!("no adapter for {}", family.name())))
}

thread_local! {
    /// The device of the screen opened on this thread: the capture thread builds its encoder on it, as the real helper
    /// builds its encoder on the capture's device.
    static DEVICE: RefCell<Option<ID3D11Device>> = const { RefCell::new(None) };
}

/// The device of the [`TimedScreen`] open on the calling thread.
pub fn capture_device() -> Option<ID3D11Device> {
    DEVICE.with(|d| d.borrow().clone())
}

struct Open {
    uploader: TextureUploader,
    pool: Vec<Vec<u8>>,
    fps: u16,
    /// The same instant on both clocks: slots are counted from it.
    base: (Instant, u64),
    /// Slots handed out or skipped so far.
    slot: u64,
    /// Frames handed out: the barcode's counter.
    counter: u32,
    /// The next frame, uploaded, with the instant it is due.
    ready: Option<(Frame, Instant)>,
}

pub struct TimedScreen {
    spec: SourceSpec,
    adapter: AdapterId,
    clock: QpcClock,
    open: Option<Open>,
    /// Processor cycles this thread spent inside `next_frame` (drawing, uploading, waiting for the slot): the
    /// source's own cost, which a real capture does not have.
    own_cycles: u64,
    cost: Option<(std::fs::File, Instant)>,
}

/// Where the screen appends `time own_cycles process_cycles` twice a second, beside the marker log.
pub fn cost_file(markers: &std::path::Path) -> PathBuf {
    markers.with_file_name("source-cost.txt")
}

fn thread_cycles() -> u64 {
    let mut cycles = 0u64;
    // SAFETY: fills a u64 for the calling thread's pseudo handle.
    let _ = unsafe { QueryThreadCycleTime(GetCurrentThread(), &mut cycles) };
    cycles
}

fn process_cycles() -> u64 {
    let mut cycles = 0u64;
    // SAFETY: fills a u64 for this process's pseudo handle.
    let _ = unsafe { QueryProcessCycleTime(GetCurrentProcess(), &mut cycles) };
    cycles
}

impl TimedScreen {
    pub fn new(spec: SourceSpec) -> Result<Self> {
        let (adapter, _) = adapter_of(spec.family)?;
        Ok(Self { spec, adapter, clock: QpcClock, open: None, own_cycles: 0, cost: None })
    }

    /// Stamps and uploads the frame of the next slot that is still ahead (a slot already past by more than a frame is
    /// skipped: the source fell behind, and the viewer sees fewer frames per second, which is what happened).
    fn prepare(&mut self) -> Result<()> {
        let (w, h) = (self.spec.width, self.spec.height);
        let Some(o) = self.open.as_mut() else { return Err(err(ErrorKind::Closed, "TimedScreen.prepare", "not open")) };
        let period_ns = 1_000_000_000u64 / u64::from(o.fps);
        let behind = Instant::now().saturating_duration_since(o.base.0).as_nanos() as u64 / period_ns;
        o.slot = o.slot.max(behind);
        let offset = Duration::from_nanos(o.slot * period_ns);
        let due = o.base.0 + offset;
        let captured_at = MonoMicros(o.base.1 + offset.as_micros() as u64);
        o.slot += 1;
        let idx = o.counter as usize % o.pool.len();
        let stride = w * 4;
        draw_barcode(&mut o.pool[idx], stride, w, h, o.counter, captured_at);
        o.counter = o.counter.wrapping_add(1);
        let cpu = Frame {
            display: DisplayId(0),
            captured_at,
            width: w,
            height: h,
            pixels: Pixels::Cpu { stride, data: std::mem::take(&mut o.pool[idx]) },
            dirty: Vec::new(),
            content_changed: true,
        };
        let gpu = o.uploader.upload(&cpu);
        let Pixels::Cpu { data, .. } = cpu.pixels else { unreachable!("made above") };
        o.pool[idx] = data;
        o.ready = Some((gpu?, due));
        Ok(())
    }
}

impl ScreenSource for TimedScreen {
    fn displays(&mut self) -> Result<Vec<DisplayInfo>> {
        Ok(vec![DisplayInfo {
            id: DisplayId(0),
            name: "Synthetic".to_string(),
            bounds: Rect { x: 0, y: 0, width: self.spec.width, height: self.spec.height },
            scale_milli: 1000,
            refresh_mhz: u32::from(self.spec.fps) * 1000,
            rotation: 0,
            primary: true,
            adapter: self.adapter,
        }])
    }

    fn open(&mut self, display: DisplayId, config: CaptureConfig) -> Result<()> {
        if display != DisplayId(0) {
            return Err(err(ErrorKind::InvalidInput, "TimedScreen.open", "unknown display"));
        }
        let (w, h) = (self.spec.width, self.spec.height);
        // The pictures come from the C7 screen (moving gradient, frame number); each hand-out stamps its own barcode.
        let mut screen = SyntheticScreen::new(w, h, self.spec.fps).with_clock(Arc::new(FakeClock::new(MonoMicros(0))));
        screen.open(DisplayId(0), CaptureConfig { max_fps: self.spec.fps })?;
        let mut pool = Vec::with_capacity(POOL);
        for _ in 0..POOL {
            let f = screen
                .next_frame(Duration::from_secs(5))?
                .ok_or_else(|| err(ErrorKind::Unavailable, "TimedScreen.open", "no synthetic frame"))?;
            let Pixels::Cpu { data, .. } = f.pixels else {
                return Err(err(ErrorKind::Unsupported, "TimedScreen.open", "synthetic frames are CPU frames"));
            };
            pool.push(data);
        }
        let uploader = TextureUploader::new(self.adapter)?;
        DEVICE.with(|d| *d.borrow_mut() = Some(uploader.device()));
        // SAFETY: raises the timer resolution of this process for the pacing sleeps; it lasts until the process ends.
        unsafe {
            windows::Win32::Media::timeBeginPeriod(1);
        }
        let base = (Instant::now() + Duration::from_millis(100), self.clock.now().0 + 100_000);
        self.cost = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(cost_file(&self.spec.markers))
            .ok()
            .map(|f| (f, Instant::now()));
        self.open = Some(Open {
            uploader,
            pool,
            fps: self.spec.fps.min(config.max_fps).max(1),
            base,
            slot: 0,
            counter: 0,
            ready: None,
        });
        Ok(())
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>> {
        let before = thread_cycles();
        let frame = self.frame(timeout);
        self.own_cycles += thread_cycles().saturating_sub(before);
        if let Some((file, written)) = self.cost.as_mut()
            && written.elapsed() >= Duration::from_millis(500)
        {
            let _ = writeln!(file, "{} {} {}", self.clock.now().0, self.own_cycles, process_cycles());
            *written = Instant::now();
        }
        frame
    }

    fn reacquire(&mut self) -> Result<()> {
        Ok(())
    }

    fn take_cursor(&mut self) -> Option<CursorUpdate> {
        None
    }

    fn close(&mut self) {
        self.open = None;
        self.cost = None;
        DEVICE.with(|d| *d.borrow_mut() = None);
    }
}

impl TimedScreen {
    /// The next frame at its slot, or `None` when the slot is further away than `timeout`.
    fn frame(&mut self, timeout: Duration) -> Result<Option<Frame>> {
        if self.open.as_ref().is_some_and(|o| o.ready.is_none()) {
            self.prepare()?;
        }
        let Some(o) = self.open.as_mut() else {
            return Err(err(ErrorKind::Closed, "TimedScreen.next_frame", "not open"));
        };
        let due = o.ready.as_ref().map(|r| r.1).expect("prepared above");
        let left = due.saturating_duration_since(Instant::now());
        if left > timeout {
            std::thread::sleep(timeout);
            return Ok(None);
        }
        if left > SPIN {
            std::thread::sleep(left - SPIN);
        }
        while Instant::now() < due {
            std::hint::spin_loop();
        }
        Ok(o.ready.take().map(|r| r.0))
    }
}

/// The C7 sine with a marker every second, paced by the wall clock, and a thread that appends each marker it played
/// to `markers` as `seq captured_at_us` (the measuring process reads the file at the end of the run).
pub fn marked_sine(markers: PathBuf) -> SineSource {
    let source = SineSource::new(Arc::new(QpcClock)).realtime();
    let log = source.log_handle();
    let spawned = std::thread::Builder::new().name("measure-markers".into()).spawn(move || {
        let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&markers) else { return };
        let mut written: Option<(u64, MonoMicros)> = None;
        // The source holds the other handle: when it is dropped the audio stopped and this thread ends.
        while Arc::strong_count(&log) > 1 {
            std::thread::sleep(Duration::from_millis(250));
            let list = log.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
            for m in list {
                // `start` clears the log, so a marker is new when it is past the last one written of this start.
                if written.is_none_or(|(seq, at)| m.captured_at > at || (m.captured_at == at && m.seq > seq)) {
                    let _ = writeln!(file, "{} {}", m.seq, m.captured_at.0);
                    written = Some((m.seq, m.captured_at));
                }
            }
            let _ = file.flush();
        }
    });
    drop(spawned);
    source
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_source_spec_survives_the_environment_with_a_path_that_holds_commas() {
        let spec = SourceSpec {
            width: 2560,
            height: 1600,
            fps: 60,
            family: Family::QuickSync,
            markers: PathBuf::from(r"C:\a folder\with,comma\markers.txt"),
        };
        assert_eq!(SourceSpec::parse(&spec.to_env()), Some(spec));
        assert_eq!(SourceSpec::parse("1920x1080@60,nvenc"), None);
        assert_eq!(SourceSpec::parse("1920x1080,nvenc,m.txt"), None);
        assert_eq!(SourceSpec::parse("1920x1080@60,x265,m.txt"), None);
    }
}
