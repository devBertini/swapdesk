//! Platform traits of Swapdesk (contract C3, `docs/CONTRACTS.md`). Blocking calls with timeouts, used from dedicated
//! OS threads; the traits are not `Send` unless stated. Std and `swapdesk-proto` only, no `unsafe`.

use core::time::Duration;
use std::sync::mpsc::SyncSender;
pub use swapdesk_proto::Codec;
use swapdesk_proto::{Action, Desktop, H264Profile, InputEvent, Preset, VideoHeader};

// ---------- errors ----------
pub type Result<T> = core::result::Result<T, PlatformError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformError {
    pub kind: ErrorKind,
    /// The operation, e.g. "DuplicateOutput", "SCStream.start".
    pub op: &'static str,
    /// HRESULT, OSStatus or errno, when there is one.
    pub os_code: Option<i64>,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// Desktop switch or mode change: call `reacquire` and go on.
    AccessLost,
    /// An OS permission is missing; the UI names it (F-FIND-04).
    PermissionDenied(OsGrant),
    Unsupported,
    /// Transient: no console session, device busy, the DDA four-process limit.
    Unavailable,
    Timeout,
    InvalidInput,
    /// The object was stopped or its peer went away.
    Closed,
    Os,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsGrant {
    ScreenRecording,
    Accessibility,
    InputMonitoring,
    ServiceApproval,
    Administrator,
}

impl core::fmt::Display for PlatformError {
    /// "<op>: <kind> (<os_code>) <detail>"; the parenthesis and the detail are left out when empty.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}: {:?}", self.op, self.kind)?;
        if let Some(code) = self.os_code {
            write!(f, " ({code})")?;
        }
        if !self.detail.is_empty() {
            write!(f, " {}", self.detail)?;
        }
        Ok(())
    }
}
impl std::error::Error for PlatformError {}

// ---------- time ----------
/// Microseconds of a monotonic clock shared by all processes of one machine
/// (QueryPerformanceCounter / mach_continuous_time). Cross-process consistency is UNPROVEN: stage 12 tests it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MonoMicros(pub u64);

pub trait Clock: Send + Sync {
    fn now(&self) -> MonoMicros;
}

// ---------- displays and frames ----------
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DisplayId(pub u16);
/// Windows: the adapter LUID; macOS: 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AdapterId(pub u64);
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayInfo {
    pub id: DisplayId,
    pub name: String,
    /// Physical pixels in the virtual desktop (may be negative).
    pub bounds: Rect,
    pub scale_milli: u16,
    pub refresh_mhz: u32,
    pub rotation: u16,
    pub primary: bool,
    pub adapter: AdapterId,
}

/// A GPU surface owned by one platform crate (ID3D11Texture2D, CVPixelBuffer);
/// only that platform's codec or renderer downcasts it.
pub struct GpuSurface(pub Box<dyn core::any::Any>);

pub enum Pixels {
    /// BGRA8, rows of `stride` bytes.
    Cpu {
        stride: u32,
        data: Vec<u8>,
    },
    Gpu(GpuSurface),
}

pub struct Frame {
    pub display: DisplayId,
    pub captured_at: MonoMicros,
    pub width: u32,
    pub height: u32,
    pub pixels: Pixels,
    /// Empty = unknown, treat as the whole frame.
    pub dirty: Vec<Rect>,
    /// False when only the cursor moved.
    pub content_changed: bool,
}

#[derive(Clone, PartialEq, Eq)]
pub struct CursorShape {
    pub id: u32,
    pub width: u16,
    pub height: u16,
    pub hot_x: u16,
    pub hot_y: u16,
    pub rgba: Vec<u8>,
}

/// The pixels are whatever is under the pointer: `Debug` shows their size only (CONTRACTS.md C2.3).
impl core::fmt::Debug for CursorShape {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CursorShape")
            .field("id", &self.id)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("hot_x", &self.hot_x)
            .field("hot_y", &self.hot_y)
            .field("rgba", &swapdesk_proto::RedactedLen(self.rgba.len()))
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorUpdate {
    pub display: DisplayId,
    /// Pixels inside the display.
    pub x: i32,
    pub y: i32,
    pub visible: bool,
    /// Present when the shape changed.
    pub shape: Option<CursorShape>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureConfig {
    pub max_fps: u16,
}

pub trait ScreenSource {
    fn displays(&mut self) -> Result<Vec<DisplayInfo>>;
    fn open(&mut self, display: DisplayId, config: CaptureConfig) -> Result<()>;
    /// Blocks up to `timeout`. Ok(None): nothing new. Err(AccessLost): call `reacquire`.
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>>;
    /// Follow the input desktop again and rebuild the capture (Windows); reopen the stream (macOS).
    fn reacquire(&mut self) -> Result<()>;
    /// The cursor change since the last call, if any; the cursor is never drawn into frames.
    fn take_cursor(&mut self) -> Option<CursorUpdate>;
    fn close(&mut self);
}

// ---------- video ----------
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u16,
    pub kbps: u32,
    pub preset: Preset,
    pub adapter: AdapterId,
    /// The codec to encode in (F-VIEW-07): H.264, or HEVC when the session picked it
    /// (`Capabilities::pick_codec`); an encoder that cannot do the codec answers `ErrorKind::Unsupported`.
    pub codec: Codec,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderInfo {
    pub name: String,
    pub hardware: bool,
    pub profile: H264Profile,
    pub level: u8,
    pub low_latency: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    pub captured_at: MonoMicros,
    pub encode_us: u32,
    pub keyframe: bool,
    /// SPS/PPS included because the configuration or size changed.
    pub config_change: bool,
    pub width: u16,
    pub height: u16,
    pub annex_b: Vec<u8>,
}

pub trait VideoEncoder {
    fn configure(&mut self, config: &EncoderConfig) -> Result<EncoderInfo>;
    /// May return nothing while the encoder fills, or more than one frame.
    /// `force_keyframe` is best effort (VideoToolbox): check `keyframe` on the output and ask again.
    fn encode(&mut self, frame: &Frame, force_keyframe: bool) -> Result<Vec<EncodedFrame>>;
    fn set_rate(&mut self, kbps: u32, fps: u16) -> Result<()>;
    /// Still-screen refinement (L4): `frame` is the last picture the screen showed and it has not changed for a while.
    /// Encodes it once as an IDR at high quality (the bits are free: the link is idle) and leaves the normal rate
    /// control in force for the next `encode`. The default does nothing (no frame), for encoders without the
    /// capability; an encoder that cannot do it returns `ErrorKind::Unsupported`, and the caller stops asking.
    fn refine(&mut self, frame: &Frame) -> Result<Vec<EncodedFrame>> {
        let _ = frame;
        Ok(Vec::new())
    }
    fn flush(&mut self) -> Result<Vec<EncodedFrame>>;
}

pub struct DecodedFrame {
    pub display: DisplayId,
    pub frame_id: u32,
    pub captured_at: MonoMicros,
    pub width: u32,
    pub height: u32,
    pub pixels: Pixels,
}

pub trait VideoDecoder {
    /// Annex B in, at most one picture out (low-latency mode, no reordering).
    fn decode(&mut self, header: &VideoHeader, annex_b: &[u8]) -> Result<Option<DecodedFrame>>;
    /// Drop state after a gap; the next input must be a keyframe.
    fn reset(&mut self);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScaleMode {
    Fit,
    OneToOne,
    Scroll { x: i32, y: i32 },
}

/// Where the video goes: a node of the shell's own composition tree (D18, research/ui.md). Raw COM / Objective-C
/// pointers as integers, owned by the shell for the renderer's lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeSurface {
    /// The DirectComposition device, the parent visual placed under the toolbar visual, and the D3D11 device the
    /// MF decoder also uses (zero-copy: NV12 → video processor → composition swap chain → `SetContent`).
    Windows { dcomp_device: u64, parent_visual: u64, d3d11_device: u64 },
    /// The parent `CALayer`; the renderer adds an `AVSampleBufferDisplayLayer` under the toolbar view's layer and
    /// enqueues through `sampleBufferRenderer` (macOS 14+) with `DisplayImmediately`.
    MacOs { parent_layer: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresentStats {
    pub presented_at: MonoMicros,
}

pub trait Renderer {
    /// Adds the video node to the shell's tree. One window: the floating toolbar is a sibling node above it, drawn by
    /// the shell (no child HWND or NSView, no airspace problem).
    fn attach(&mut self, surface: NativeSurface, area: Rect) -> Result<()>;
    /// `area` is the video rectangle in window pixels, after the shell's layout.
    fn resize(&mut self, area: Rect) -> Result<()>;
    fn present(&mut self, frame: DecodedFrame, scale: ScaleMode) -> Result<PresentStats>;
    fn set_cursor(&mut self, cursor: &CursorUpdate) -> Result<()>;
    /// Pointer events come from the shell's window; the shell asks the renderer where a window point falls in the
    /// shown display: PROTOCOL.md 7.3 coordinates, or None outside the picture (letterbox, scrolled out).
    fn map_pointer(&self, x: i32, y: i32) -> Option<(u16, u16)>;
    fn detach(&mut self);
}

// ---------- audio ----------
/// 48 kHz, stereo, interleaved f32; 10 ms = 960 values.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioBlock {
    pub captured_at: MonoMicros,
    pub samples: Vec<f32>,
}

pub trait AudioSource {
    /// `exclude_pid`: leave this process tree out (Windows process loopback, build 20348+; windows.md 6b).
    fn start(&mut self, exclude_pid: Option<u32>) -> Result<()>;
    fn next_block(&mut self, timeout: Duration) -> Result<Option<AudioBlock>>;
    fn stop(&mut self);
}

pub trait AudioSink {
    fn start(&mut self) -> Result<()>;
    fn write(&mut self, block: &AudioBlock) -> Result<()>;
    /// Audio written but not yet played.
    fn queued(&self) -> Duration;
    fn stop(&mut self);

    /// True when the sink is paced by its own device clock: `wait_room` blocks until the device wants more audio, and
    /// the caller writes exactly what it reports (W1.2). A sink that is not (the default) is written on the caller's
    /// own 10 ms tick.
    fn device_paced(&self) -> bool {
        false
    }

    /// Device-paced sinks only, after `start`: waits up to `timeout` for the device to ask for audio and returns how
    /// many whole 10 ms frames fit in the queue below the sink's fill mark (0 on a timeout).
    fn wait_room(&mut self, _timeout: Duration) -> Result<usize> {
        Ok(0)
    }

    /// Device-paced sinks: writes one frame of silence to prime the queue after `start`, so the device does not run
    /// dry before the first frame; the default does nothing.
    fn prime(&mut self) -> Result<()> {
        Ok(())
    }

    /// Counts the sink keeps instead of dropping silently.
    fn counts(&self) -> SinkCounts {
        SinkCounts::default()
    }
}

/// What an audio sink counted since `start`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SinkCounts {
    /// Device wake-ups that found the queue empty while the stream was running (a cut the listener hears).
    pub underruns: u64,
    /// Frames that did not fit in the device queue and were dropped.
    pub overrun_frames: u64,
}

// ---------- input ----------
pub trait InputSink {
    fn displays_changed(&mut self, displays: &[DisplayInfo]);
    /// Effects are checked by tests, never the return value alone (SendInput fails silently under UIPI, windows.md 5a).
    /// Windows: the user helper's sink (medium-integrity user token) serves prompted sessions and the system helper's
    /// sink (SYSTEM) serves only UNATTENDED sessions (D11); the session layer picks the sink, never the sink itself.
    fn apply(&mut self, event: &InputEvent) -> Result<()>;
    fn release_all(&mut self) -> Result<()>;
    /// Windows: OpenInputDesktop + SetThreadDesktop on the calling thread. macOS: nothing.
    fn follow_input_desktop(&mut self) -> Result<()>;
}

pub trait InputGrabber {
    /// Keyboard of the viewer while its window has focus: key records (and text records in text mode) go to `events`.
    fn start(&mut self, events: SyncSender<InputEvent>) -> Result<()>;
    /// The shell calls `start` when the viewer window gains focus and `stop` when it loses it (F-INPUT-04).
    /// Swallow system shortcuts (Alt+Tab, Win, Cmd+Tab) and send them instead.
    fn set_grab_system_keys(&mut self, on: bool) -> Result<()>;
    fn stop(&mut self);
}

// ---------- clipboard ----------
#[derive(Clone, PartialEq, Eq)]
pub enum ClipContent {
    Text(String),
    Png(Vec<u8>),
}

/// Clipboard content is never logged (CONTRACTS.md C2.3): `Debug` shows the kind and the size only.
impl core::fmt::Debug for ClipContent {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Text(t) => write!(f, "Text({:?})", swapdesk_proto::RedactedLen(t.len())),
            Self::Png(p) => write!(f, "Png({:?})", swapdesk_proto::RedactedLen(p.len())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipRead {
    Content(ClipContent),
    Concealed,
    Empty,
    Unsupported,
}

pub trait ClipboardProvider {
    /// Changes whenever the clipboard changes (sequence number / changeCount); polled.
    fn change_count(&mut self) -> Result<u64>;
    fn read(&mut self) -> Result<ClipRead>;
    fn write(&mut self, content: &ClipContent) -> Result<()>;
}

// ---------- terminal ----------
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtyRequest {
    pub cols: u16,
    pub rows: u16,
    pub admin: bool,
}

pub trait Pty: Send {
    /// The account the shell runs as ("alex", "SYSTEM", "root").
    fn account(&self) -> &str;
    fn reader(&mut self) -> Result<Box<dyn std::io::Read + Send>>;
    fn writer(&mut self) -> Result<Box<dyn std::io::Write + Send>>;
    fn resize(&mut self, cols: u16, rows: u16) -> Result<()>;
    fn try_wait(&mut self) -> Result<Option<i32>>;
    fn kill(&mut self) -> Result<()>;
}

pub trait PtySpawner: Send + Sync {
    /// Own code (D14): ConPTY (`CreatePseudoConsole`) in `swapdesk-sys-win`, `openpty` in `swapdesk-sys-mac`.
    /// Spawns as the account of the calling process; `admin` is refused by a spawner that is not SYSTEM or root.
    /// The admin spawner runs only in the service (session 0) or the daemon, never in a process on the user's desktop.
    fn spawn(&self, request: &PtyRequest) -> Result<Box<dyn Pty>>;
}

// ---------- session, presence, actions ----------
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleState {
    pub session_id: Option<u32>,
    pub user: Option<String>,
    pub user_is_admin: bool,
    pub desktop: Desktop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    ConsoleChanged(ConsoleState),
    DesktopSwitched(Desktop),
    DisplaysChanged,
    Suspending,
    Resumed,
    NetworkChanged,
}

pub trait SessionWatcher {
    fn state(&mut self) -> Result<ConsoleState>;
    fn next_event(&mut self, timeout: Duration) -> Result<Option<SessionEvent>>;
}

pub trait ActionRunner {
    /// Lock, Sleep, Restart, TaskMonitor (Task Manager / Activity Monitor). The user helper / Aqua agent runs every action;
    /// with nobody logged in, the service/daemon runs Lock, Sleep and Restart as system calls and refuses TaskMonitor.
    /// No implementation starts a GUI process as SYSTEM or root.
    fn run(&mut self, action: Action) -> Result<()>;
    /// Keep the machine awake during a session.
    fn keep_awake(&mut self, on: bool) -> Result<()>;
}

// ---------- network facts for the LAN guard ----------
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IfKind {
    Ethernet,
    WiFi,
    Virtual,
    Vpn,
    Tunnel,
    Loopback,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetCategory {
    Public,
    Private,
    Domain,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetInterface {
    pub index: u32,
    pub name: String,
    pub kind: IfKind,
    pub up: bool,
    pub mac: Option<[u8; 6]>,
    pub ipv4: Vec<(std::net::Ipv4Addr, u8)>,
    /// Windows only (NLM); None on macOS.
    pub category: Option<NetCategory>,
    /// Windows firewall state for this profile; None when unknown.
    pub inbound_blocked: Option<bool>,
}

pub trait NetInterfaces: Send + Sync {
    fn list(&self) -> Result<Vec<NetInterface>>;
}

// ---------- trace log (own logger, D14) ----------
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
}

pub trait TraceSink: Send + Sync {
    /// `what` must never hold a never-logged value (C2.3); `swapdesk-fakes::LogCapture` checks it.
    fn log(&self, level: Level, target: &'static str, what: core::fmt::Arguments<'_>);
}

// ---------- secrets (C5) ----------
pub trait SecretStore: Send + Sync {
    fn load(&self, name: &str) -> Result<Option<Vec<u8>>>;
    fn store(&self, name: &str, bytes: &[u8]) -> Result<()>;
    fn delete(&self, name: &str) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_of_a_cursor_shape_and_an_update_holding_it_shows_the_size_and_never_the_pixels() {
        let shape = CursorShape { id: 1, width: 2, height: 2, hot_x: 0, hot_y: 0, rgba: vec![233; 16] };
        let update = CursorUpdate { display: DisplayId(0), x: 5, y: 6, visible: true, shape: Some(shape.clone()) };
        for shown in [format!("{shape:?}"), format!("{update:?}")] {
            assert!(!shown.split(|c: char| !c.is_ascii_digit()).any(|t| t == "233"), "pixels leaked: {shown}");
            assert!(shown.contains("<redacted 16 bytes>"), "{shown}");
        }
    }

    #[test]
    fn debug_of_clipboard_content_shows_the_kind_and_size_and_never_the_content() {
        let text = format!("{:?}", ClipRead::Content(ClipContent::Text("senha123".into())));
        assert!(!text.contains("senha"), "clipboard text leaked: {text}");
        assert!(text.contains("Text(<redacted 8 bytes>)"), "{text}");
        let png = format!("{:?}", ClipContent::Png(vec![233; 12]));
        assert!(!png.split(|c: char| !c.is_ascii_digit()).any(|t| t == "233"), "{png}");
        assert!(png.contains("Png(<redacted 12 bytes>)"), "{png}");
    }

    /// Compiles only when every trait that C3 uses behind `dyn` is object-safe.
    #[allow(clippy::too_many_arguments)]
    fn _object_safe(
        _: &dyn Clock,
        _: &mut dyn ScreenSource,
        _: &mut dyn VideoEncoder,
        _: &mut dyn VideoDecoder,
        _: &mut dyn Renderer,
        _: &mut dyn AudioSource,
        _: &mut dyn AudioSink,
        _: &mut dyn InputSink,
        _: &mut dyn InputGrabber,
        _: &mut dyn ClipboardProvider,
        _: &mut dyn Pty,
        _: &dyn PtySpawner,
        _: &mut dyn SessionWatcher,
        _: &mut dyn ActionRunner,
        _: &dyn NetInterfaces,
        _: &dyn TraceSink,
        _: &dyn SecretStore,
    ) {
    }

    struct FixedClock(u64);

    impl Clock for FixedClock {
        fn now(&self) -> MonoMicros {
            MonoMicros(self.0)
        }
    }

    struct Capture(std::sync::Mutex<Vec<String>>);

    impl TraceSink for Capture {
        fn log(&self, level: Level, target: &'static str, what: core::fmt::Arguments<'_>) {
            self.0.lock().unwrap().push(format!("{level:?} {target} {what}"));
        }
    }

    #[test]
    fn a_boxed_clock_and_sink_work_through_dyn() {
        let clock: Box<dyn Clock> = Box::new(FixedClock(42));
        assert_eq!(clock.now(), MonoMicros(42));
        let sink = Capture(Default::default());
        let dynamic: &dyn TraceSink = &sink;
        dynamic.log(Level::Warn, "test", format_args!("n={}", 3));
        assert_eq!(sink.0.lock().unwrap().as_slice(), ["Warn test n=3"]);
    }

    #[test]
    fn platform_error_display_follows_the_contract() {
        let full = PlatformError {
            kind: ErrorKind::AccessLost,
            op: "DuplicateOutput",
            os_code: Some(-2005270526),
            detail: "desktop switch".into(),
        };
        assert_eq!(full.to_string(), "DuplicateOutput: AccessLost (-2005270526) desktop switch");
        let bare = PlatformError { kind: ErrorKind::Closed, op: "stop", os_code: None, detail: String::new() };
        assert_eq!(bare.to_string(), "stop: Closed");
        let _: &dyn std::error::Error = &bare;
    }

    #[test]
    fn a_gpu_surface_is_downcast_by_its_owner() {
        let surface = GpuSurface(Box::new(7u32));
        assert_eq!(surface.0.downcast_ref::<u32>(), Some(&7));
        assert!(surface.0.downcast_ref::<u64>().is_none());
    }
}
