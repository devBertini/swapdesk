//! `sck`: ScreenCaptureKit main-display capture, frame and audio counters (P4, P5).

use crate::d;
use crate::log::emit;
use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AnyThread, DefinedClass};
use objc2_core_graphics::CGMainDisplayID;
use objc2_core_media::{CMSampleBuffer, CMTime, CMTimeFlags};
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamDelegate, SCStreamOutput, SCStreamOutputType, SCWindow,
};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

/// `kCVPixelFormatType_32BGRA` ('BGRA').
const PIXEL_FORMAT_BGRA: u32 = 0x4247_5241;

#[derive(Default)]
pub struct Stats {
    pub frames: AtomicU64,
    pub audio_buffers: AtomicU64,
    /// Peak absolute sample as f32 bits (non-negative floats order like their bits).
    pub audio_peak_bits: AtomicU32,
    pub last_w: AtomicU64,
    pub last_h: AtomicU64,
    /// Mean luma of the last sampled frame, times 1000.
    pub last_luma_milli: AtomicU64,
    pub stopped_with_error: AtomicBool,
    pub stop_error: std::sync::Mutex<String>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = AnyThread]
    #[name = "SwapdeskProbeStreamHandler"]
    #[ivars = Arc<Stats>]
    pub struct Handler;

    unsafe impl NSObjectProtocol for Handler {}

    unsafe impl SCStreamOutput for Handler {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn stream_did_output_sample_buffer_of_type(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            let stats = self.ivars();
            if kind == SCStreamOutputType::Screen {
                // Idle frames carry no image buffer; count only real frames.
                // SAFETY: image_buffer only reads the sample buffer handed to the callback.
                let Some(image) = (unsafe { sample_buffer.image_buffer() }) else { return };
                let n = stats.frames.fetch_add(1, Ordering::Relaxed);
                if n.is_multiple_of(10) {
                    sample_luma(&image, stats);
                }
            } else if kind == SCStreamOutputType::Audio {
                stats.audio_buffers.fetch_add(1, Ordering::Relaxed);
                // GUESS: SCK audio is Float32 PCM and the block buffer holds the samples back to back
                // (planar channels concatenated), so the peak over all floats is a valid peak.
                // SAFETY: data_buffer/data_pointer read the buffer handed to the callback.
                unsafe {
                    if let Some(block) = sample_buffer.data_buffer() {
                        let mut len = 0usize;
                        let mut ptr: *mut std::ffi::c_char = std::ptr::null_mut();
                        let status = block.data_pointer(0, std::ptr::null_mut(), &mut len, &mut ptr);
                        if status == 0 && !ptr.is_null() {
                            let floats = std::slice::from_raw_parts(ptr as *const f32, len / 4);
                            let peak = floats.iter().fold(0f32, |m, v| m.max(v.abs()));
                            stats.audio_peak_bits.fetch_max(peak.to_bits(), Ordering::Relaxed);
                        }
                    }
                }
            }
        }
    }

    unsafe impl SCStreamDelegate for Handler {
        #[unsafe(method(stream:didStopWithError:))]
        unsafe fn stream_did_stop_with_error(&self, _stream: &SCStream, error: &NSError) {
            let stats = self.ivars();
            stats.stopped_with_error.store(true, Ordering::Relaxed);
            if let Ok(mut s) = stats.stop_error.lock() {
                *s = error.localizedDescription().to_string();
            }
        }
    }
);

impl Handler {
    fn new(stats: Arc<Stats>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(stats);
        // SAFETY: NSObject's init on a freshly allocated and initialised-ivars object.
        unsafe { msg_send![super(this), init] }
    }
}

fn sample_luma(image: &objc2_core_video::CVImageBuffer, stats: &Stats) {
    // SAFETY: the buffer is locked read-only while its bytes are read and unlocked before returning.
    unsafe {
        let flags = CVPixelBufferLockFlags::ReadOnly;
        if CVPixelBufferLockBaseAddress(image, flags) != 0 {
            return;
        }
        let (w, h) = (CVPixelBufferGetWidth(image), CVPixelBufferGetHeight(image));
        let stride = CVPixelBufferGetBytesPerRow(image);
        let base = CVPixelBufferGetBaseAddress(image) as *const u8;
        if !base.is_null() && w > 0 && h > 0 {
            let (mut sum, mut count) = (0u64, 0u64);
            for y in (0..h).step_by(16) {
                for x in (0..w).step_by(16) {
                    let p = base.add(y * stride + x * 4); // BGRA
                    let (b, g, r) = (u64::from(*p), u64::from(*p.add(1)), u64::from(*p.add(2)));
                    sum += (299 * r + 587 * g + 114 * b) / 1000;
                    count += 1;
                }
            }
            if let Some(mean) = (sum * 1000).checked_div(count) {
                stats.last_luma_milli.store(mean, Ordering::Relaxed);
            }
            stats.last_w.store(w as u64, Ordering::Relaxed);
            stats.last_h.store(h as u64, Ordering::Relaxed);
        }
        CVPixelBufferUnlockBaseAddress(image, flags);
    }
}

struct SendDisplay(Retained<SCDisplay>);
// SAFETY: the SCDisplay is only moved to the thread that waits for it and used there; ScreenCaptureKit
// objects are documented as thread-safe for this use (GUESS, not stated in the generated docs).
unsafe impl Send for SendDisplay {}

fn error_text(e: *mut NSError) -> String {
    // SAFETY: the completion handler passes either null or a valid NSError for the call's duration.
    unsafe { e.as_ref() }.map_or_else(String::new, |e| {
        format!("{} (domain {}, code {})", e.localizedDescription(), e.domain(), e.code())
    })
}

/// Main display via `SCShareableContent` with a 10 s timeout, logging what it saw.
fn main_display() -> Option<Retained<SCDisplay>> {
    let (tx, rx) = mpsc::channel::<Result<(SendDisplay, usize, usize), String>>();
    let block = RcBlock::new(move |content: *mut SCShareableContent, err: *mut NSError| {
        // SAFETY: SCK passes a valid content object or null.
        let Some(content) = (unsafe { content.as_ref() }) else {
            let _ = tx.send(Err(error_text(err)));
            return;
        };
        // SAFETY: plain property reads on a live object.
        let (displays, apps, windows) = unsafe { (content.displays(), content.applications(), content.windows()) };
        let main_id = CGMainDisplayID();
        let chosen = displays
            .iter()
            .find(|dsp| unsafe { dsp.displayID() } == main_id)
            .or_else(|| displays.firstObject());
        match chosen {
            Some(dsp) => {
                let _ = tx.send(Ok((SendDisplay(dsp), apps.count(), windows.count())));
            }
            None => {
                let _ = tx.send(Err(format!("no displays (apps {}, windows {})", apps.count(), windows.count())));
            }
        }
    });
    // SAFETY: the block is valid for the call; SCK copies it.
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&block) };
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok((SendDisplay(display), apps, windows))) => {
            // SAFETY: property reads.
            let (id, w, h) = unsafe { (display.displayID(), display.width(), display.height()) };
            emit(
                "sck.shareable",
                true,
                &d!("display_id" => id, "width_pt" => w as i64, "height_pt" => h as i64, "apps" => apps, "windows" => windows),
            );
            Some(display)
        }
        Ok(Err(e)) => {
            emit("sck.shareable", false, &d!("error" => e));
            None
        }
        Err(_) => {
            emit("sck.shareable", false, &d!("error" => "timeout after 10 s (no completion handler call)"));
            None
        }
    }
}

fn wait_error(rx: &mpsc::Receiver<String>) -> Result<(), String> {
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(s) if s.is_empty() => Ok(()),
        Ok(s) => Err(s),
        Err(_) => Err("timeout after 10 s".to_string()),
    }
}

pub fn run(seconds: u64, audio: bool) {
    // GUESS: a process with no NSApplication must open a window-server connection before SCK is used
    // ("CGS_REQUIRE_INIT"); touching CGMainDisplayID does that.
    let _ = CGMainDisplayID();
    let Some(display) = main_display() else { return };

    // SAFETY: ordinary Objective-C object construction and configuration.
    let (stream, handler, stats) = unsafe {
        let no_windows: Retained<NSArray<SCWindow>> = NSArray::new();
        let filter = SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), &display, &no_windows);
        let config = SCStreamConfiguration::new();
        config.setWidth((display.width().max(1) as usize) * 2);
        config.setHeight((display.height().max(1) as usize) * 2);
        config.setPixelFormat(PIXEL_FORMAT_BGRA);
        config.setQueueDepth(5);
        config.setShowsCursor(true);
        config.setMinimumFrameInterval(CMTime { value: 1, timescale: 30, flags: CMTimeFlags::Valid, epoch: 0 });
        if audio {
            config.setCapturesAudio(true);
            config.setExcludesCurrentProcessAudio(true);
        }
        let stats = Arc::new(Stats::default());
        let handler = Handler::new(stats.clone());
        let delegate = ProtocolObject::from_ref(&*handler);
        let stream = SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &config, Some(delegate));
        (stream, handler, stats)
    };
    let queue = DispatchQueue::new("local.swapdesk.probe.sck", None);
    let output = ProtocolObject::from_ref(&*handler);
    // SAFETY: the handler outlives the stream (both live until the end of this function).
    unsafe {
        if let Err(e) = stream.addStreamOutput_type_sampleHandlerQueue_error(output, SCStreamOutputType::Screen, Some(&queue)) {
            emit("sck.add_output", false, &d!("error" => e.localizedDescription().to_string()));
            return;
        }
        if audio {
            if let Err(e) = stream.addStreamOutput_type_sampleHandlerQueue_error(output, SCStreamOutputType::Audio, Some(&queue)) {
                emit("sck.add_output_audio", false, &d!("error" => e.localizedDescription().to_string()));
            }
        }
    }

    let (tx, rx) = mpsc::channel::<String>();
    let start_block = RcBlock::new(move |err: *mut NSError| {
        let _ = tx.send(error_text(err));
    });
    // SAFETY: the block is valid for the call; SCK copies it.
    unsafe { stream.startCaptureWithCompletionHandler(Some(&start_block)) };
    if let Err(e) = wait_error(&rx) {
        // Typical here: user declined or never granted Screen Recording (TCC), or no session.
        emit("sck.start", false, &d!("error" => e));
        return;
    }
    emit("sck.start", true, &d!("seconds" => seconds, "audio" => audio));

    std::thread::sleep(Duration::from_secs(seconds));

    let (tx2, rx2) = mpsc::channel::<String>();
    let stop_block = RcBlock::new(move |err: *mut NSError| {
        let _ = tx2.send(error_text(err));
    });
    // SAFETY: as above.
    unsafe { stream.stopCaptureWithCompletionHandler(Some(&stop_block)) };
    let stop = wait_error(&rx2);

    let frames = stats.frames.load(Ordering::Relaxed);
    emit(
        "sck.frames",
        frames > 0,
        &d!(
            "frames" => frames,
            "width" => stats.last_w.load(Ordering::Relaxed),
            "height" => stats.last_h.load(Ordering::Relaxed),
            "mean_luma" => stats.last_luma_milli.load(Ordering::Relaxed) as f64 / 1000.0,
            "stopped_with_error" => stats.stopped_with_error.load(Ordering::Relaxed),
            "stop_error" => stats.stop_error.lock().map(|s| s.clone()).unwrap_or_default(),
            "stop_call" => stop.err().unwrap_or_default()
        ),
    );
    if audio {
        let buffers = stats.audio_buffers.load(Ordering::Relaxed);
        emit(
            "sck.audio",
            buffers > 0,
            &d!("audio_buffers" => buffers, "peak" => f64::from(f32::from_bits(stats.audio_peak_bits.load(Ordering::Relaxed)))),
        );
    }
}
