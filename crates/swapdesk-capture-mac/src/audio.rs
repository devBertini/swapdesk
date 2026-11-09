//! `AudioSource` over ScreenCaptureKit (stage 24, lane audio): system audio of the main display as 48 kHz stereo
//! interleaved `f32`, in blocks of 10 ms (960 values), the app's own audio left out.
//!
//! The stream is an `SCStream` over the main display's filter with `capturesAudio`, 48000 Hz, 2 channels and
//! `excludesCurrentProcessAudio`, a 2x2 video configuration at one frame per second (SCStream insists on a video side) and
//! ONLY an audio `SCStreamOutput` on a serial dispatch queue. The output turns every `CMSampleBuffer` into interleaved
//! `f32` and appends it to a bounded ring (200 ms, oldest dropped and counted when full); `next_block` frames the ring
//! into 960-value blocks and waits on a condition variable.
//!
//! Facts from the Apple pages (read 08/11/2026):
//! * `SCStreamConfiguration.excludesCurrentProcessAudio` excludes the audio of the app that runs the stream; there is no
//!   way to name another process, so `start(Some(pid))` ignores the pid (the helper passes its own parent: the app is the
//!   process whose audio would loop back, and it is the one excluded here)
//!   (developer.apple.com/documentation/screencapturekit/scstreamconfiguration/excludescurrentprocessaudio).
//! * `SCStreamOutputType.audio` sample buffers wrap an audio buffer list in the format set by `sampleRate` and
//!   `channelCount` (developer.apple.com/documentation/screencapturekit/scstreamoutputtype); SCK delivers 32-bit float,
//!   non-interleaved. Interleaved and 16-bit integer layouts are handled too.
//! * `CGPreflightScreenCaptureAccess` tells whether the Screen Recording grant exists without prompting
//!   (developer.apple.com/documentation/coregraphics/cgpreflightscreencaptureaccess()); the request API is never called.
//! * `SCStreamError` codes (developer.apple.com/documentation/screencapturekit/scstreamerror/code).
//!
//! The pure parts (sample conversion, channel layout, the ring) build and are tested on every host; the stream itself is
//! macOS only.

use std::collections::VecDeque;

use swapdesk_platform::{ErrorKind, OsGrant};

/// Frames (per channel) in one block: 10 ms at 48 kHz.
pub const BLOCK_FRAMES: usize = 480;
/// Values in one block (interleaved stereo).
pub const BLOCK_VALUES: usize = BLOCK_FRAMES * 2;
/// The ring holds about 200 ms.
pub const RING_VALUES: usize = 20 * BLOCK_VALUES;
const SAMPLE_RATE: u64 = 48_000;

// ---------- pure parts ----------

/// How one audio buffer of a sample buffer stores its samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleKind {
    F32,
    I16,
}

const FORMAT_LINEAR_PCM: u32 = 0x6c70_636d;
const FLAG_FLOAT: u32 = 1 << 0;
const FLAG_BIG_ENDIAN: u32 = 1 << 1;
const FLAG_SIGNED_INT: u32 = 1 << 2;

/// The sample kind of an `AudioStreamBasicDescription` (format id, flags, bits per channel), when it is one this
/// source converts: native-endian linear PCM, 32-bit float or 16-bit signed integer.
pub fn sample_kind(format_id: u32, flags: u32, bits: u32) -> Option<SampleKind> {
    if format_id != FORMAT_LINEAR_PCM || flags & FLAG_BIG_ENDIAN != 0 {
        return None;
    }
    if flags & FLAG_FLOAT != 0 && bits == 32 {
        Some(SampleKind::F32)
    } else if flags & FLAG_SIGNED_INT != 0 && bits == 16 {
        Some(SampleKind::I16)
    } else {
        None
    }
}

/// 16-bit signed integer samples as floats in [-1, 1).
pub fn f32_from_i16(samples: &[i16]) -> Vec<f32> {
    samples.iter().map(|&s| f32::from(s) / 32_768.0).collect()
}

/// One audio buffer, already as `f32`: `channels` interleaved channels (1 for each plane of a non-interleaved layout).
#[derive(Debug, Clone, Copy)]
pub struct PcmBuffer<'a> {
    pub samples: &'a [f32],
    pub channels: usize,
}

/// Stereo interleaved samples from any layout: the first two channels found across the buffers become left and right
/// (two planes, one interleaved stereo buffer, or the first two channels of a wider one); a single channel is
/// duplicated into both. Only whole frames are kept (the shortest source decides). Nothing usable gives an empty vector.
pub fn to_stereo(buffers: &[PcmBuffer<'_>]) -> Vec<f32> {
    let mut sources: Vec<(&[f32], usize, usize)> = Vec::with_capacity(2);
    'find: for b in buffers {
        if b.channels == 0 {
            continue;
        }
        for ch in 0..b.channels {
            sources.push((b.samples, b.channels, ch));
            if sources.len() == 2 {
                break 'find;
            }
        }
    }
    let Some(&left) = sources.first() else {
        return Vec::new();
    };
    let right = sources.get(1).copied().unwrap_or(left);
    let frames = (left.0.len() / left.1).min(right.0.len() / right.1);
    let mut out = Vec::with_capacity(frames * 2);
    for i in 0..frames {
        out.push(left.0[i * left.1 + left.2]);
        out.push(right.0[i * right.1 + right.2]);
    }
    out
}

/// Microseconds of audio in `values` interleaved stereo values.
fn values_us(values: usize) -> u64 {
    (values / 2) as u64 * 1_000_000 / SAMPLE_RATE
}

/// The bounded queue between the SCK callback and `next_block`: interleaved stereo values, the oldest dropped when
/// more than the capacity arrives, framed into 960-value blocks.
#[derive(Debug)]
pub struct BlockRing {
    values: VecDeque<f32>,
    capacity: usize,
    /// Arrival time (micros) of the last value queued, never moving back.
    end_us: u64,
    dropped_values: u64,
}

impl BlockRing {
    /// `capacity` in values, rounded down to whole frames (at least one block).
    pub fn new(capacity: usize) -> Self {
        let capacity = (capacity & !1).max(BLOCK_VALUES);
        Self { values: VecDeque::with_capacity(capacity), capacity, end_us: 0, dropped_values: 0 }
    }

    /// Appends `chunk` (interleaved stereo, an odd trailing value is ignored) that arrived at `now_us`.
    pub fn push(&mut self, chunk: &[f32], now_us: u64) {
        let mut chunk = &chunk[..chunk.len() & !1];
        self.end_us = self.end_us.max(now_us);
        if chunk.len() > self.capacity {
            self.dropped_values += (chunk.len() - self.capacity) as u64;
            chunk = &chunk[chunk.len() - self.capacity..];
        }
        let overflow = (self.values.len() + chunk.len()).saturating_sub(self.capacity);
        if overflow > 0 {
            self.values.drain(..overflow);
            self.dropped_values += overflow as u64;
        }
        self.values.extend(chunk.iter().copied());
    }

    /// The next 960 values and the time of the first one: the arrival of the newest value minus the audio queued
    /// behind it.
    pub fn pop_block(&mut self) -> Option<(u64, Vec<f32>)> {
        if self.values.len() < BLOCK_VALUES {
            return None;
        }
        let at = self.end_us.saturating_sub(values_us(self.values.len()));
        Some((at, self.values.drain(..BLOCK_VALUES).collect()))
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Values dropped so far because the ring was full.
    pub fn dropped_values(&self) -> u64 {
        self.dropped_values
    }

    pub fn clear(&mut self) {
        self.values.clear();
    }
}

/// The `ErrorKind` for an `SCStreamError` code that stopped the stream. The system stopping the stream (display sleep,
/// configuration change) is what a device invalidation is on Windows (`AccessLost`, as `WasapiLoopbackSource` reports
/// it); the user stopping it or a stop in progress is `Closed`.
pub fn kind_for_stream_code(code: i64) -> ErrorKind {
    match code {
        -3821 => ErrorKind::AccessLost,                                  // systemStoppedStream
        -3817 | -3816 | -3808 => ErrorKind::Closed, // userStopped, removingStream, attemptToStopStreamState
        -3801 => ErrorKind::PermissionDenied(OsGrant::ScreenRecording), // userDeclined
        -3814 | -3815 | -3813 | -3802 | -3818 => ErrorKind::Unavailable, // no display / source, failed to start (audio)
        _ => ErrorKind::Os,
    }
}

#[cfg(target_os = "macos")]
pub use mac::MacAudio;

#[cfg(target_os = "macos")]
mod mac {
    use std::ptr::{self, NonNull};
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::sync::{Arc, Condvar, Mutex, MutexGuard};
    use std::time::{Duration, Instant};

    use block2::RcBlock;
    use dispatch2::{DispatchQueue, DispatchQueueAttr};
    use objc2::rc::Retained;
    use objc2::runtime::{NSObject, ProtocolObject};
    use objc2::{AnyThread, DefinedClass, define_class, msg_send};
    use objc2_core_audio_types::AudioBufferList;
    use objc2_core_foundation::CFRetained;
    use objc2_core_graphics::CGPreflightScreenCaptureAccess;
    use objc2_core_media::{CMAudioFormatDescriptionGetStreamBasicDescription, CMBlockBuffer, CMSampleBuffer, CMTime};
    use objc2_foundation::{NSArray, NSError, NSObjectProtocol};
    use objc2_screen_capture_kit::{
        SCContentFilter, SCDisplay, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamDelegate,
        SCStreamOutput, SCStreamOutputType, SCWindow,
    };
    use swapdesk_platform::{AudioBlock, AudioSource, Clock, ErrorKind, MonoMicros, OsGrant, PlatformError, Result};

    use super::{
        BlockRing, PcmBuffer, RING_VALUES, SampleKind, f32_from_i16, kind_for_stream_code, sample_kind, to_stereo,
    };

    /// How long the asynchronous SCK calls (content query, start, stop) may take.
    const CALL_TIMEOUT: Duration = Duration::from_secs(10);

    fn failure(kind: ErrorKind, op: &'static str, detail: impl Into<String>) -> PlatformError {
        PlatformError { kind, op, os_code: None, detail: detail.into() }
    }

    fn sck_error(op: &'static str, code: i64, detail: String) -> PlatformError {
        PlatformError { kind: kind_for_stream_code(code), op, os_code: Some(code), detail }
    }

    fn lock(m: &Mutex<State>) -> MutexGuard<'_, State> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    struct State {
        ring: BlockRing,
        /// The error that stopped the stream, handed out once the queued blocks are read.
        error: Option<PlatformError>,
        running: bool,
    }

    struct Shared {
        clock: Arc<dyn Clock>,
        state: Mutex<State>,
        wake: Condvar,
    }

    struct OutputIvars {
        shared: Arc<Shared>,
    }

    define_class!(
        // SAFETY: NSObject has no subclassing requirements; the class has no `Drop` and its ivars are `Send + Sync`
        // data behind an `Arc`, touched only through `&self`.
        #[unsafe(super = NSObject)]
        #[name = "SwapdeskAudioOutput"]
        #[ivars = OutputIvars]
        struct AudioOutput;

        // SAFETY: `NSObjectProtocol` has no safety requirements.
        unsafe impl NSObjectProtocol for AudioOutput {}

        // SAFETY: the selector and signature are those of `SCStreamOutput`.
        unsafe impl SCStreamOutput for AudioOutput {
            #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
            fn did_output(&self, _stream: &SCStream, sample_buffer: &CMSampleBuffer, kind: SCStreamOutputType) {
                if kind != SCStreamOutputType::Audio {
                    return;
                }
                if let Some(stereo) = convert_sample_buffer(sample_buffer) {
                    self.ivars().shared.push(&stereo);
                }
            }
        }

        // SAFETY: the selector and signature are those of `SCStreamDelegate`.
        unsafe impl SCStreamDelegate for AudioOutput {
            #[unsafe(method(stream:didStopWithError:))]
            fn did_stop(&self, _stream: &SCStream, error: &NSError) {
                let code = error.code() as i64;
                let detail = error.localizedDescription().to_string();
                self.ivars().shared.fail(sck_error("SCStream.didStopWithError", code, detail));
            }
        }
    );

    impl AudioOutput {
        fn new(shared: Arc<Shared>) -> Retained<Self> {
            let this = Self::alloc().set_ivars(OutputIvars { shared });
            // SAFETY: `init` of NSObject on a freshly allocated instance with its ivars set.
            unsafe { msg_send![super(this), init] }
        }
    }

    impl Shared {
        fn push(&self, stereo: &[f32]) {
            let now = self.clock.now().0;
            let mut st = lock(&self.state);
            if st.running {
                st.ring.push(stereo, now);
                self.wake.notify_all();
            }
        }

        fn fail(&self, e: PlatformError) {
            let mut st = lock(&self.state);
            if st.running && st.error.is_none() {
                st.error = Some(e);
            }
            self.wake.notify_all();
        }
    }

    /// The `CMSampleBuffer` of an audio stream as interleaved stereo `f32`, `None` when it holds nothing usable.
    fn convert_sample_buffer(sb: &CMSampleBuffer) -> Option<Vec<f32>> {
        // SAFETY: all calls take the live sample buffer SCK passed to the callback; pointers handed to
        // CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer point at locals or at a buffer sized as it asked, and the
        // retained block buffer keeps the audio data alive until it is dropped at the end of this function.
        unsafe {
            if !sb.data_is_ready() {
                return None;
            }
            let fd = sb.format_description()?;
            let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(&fd);
            if asbd.is_null() {
                return None;
            }
            let asbd = *asbd;
            let kind = sample_kind(asbd.mFormatID, asbd.mFormatFlags, asbd.mBitsPerChannel)?;

            let mut needed = 0usize;
            // The first call only asks for the size (it answers "array too small" with it).
            let _ = sb.audio_buffer_list_with_retained_block_buffer(
                &mut needed,
                ptr::null_mut(),
                0,
                None,
                None,
                0,
                ptr::null_mut(),
            );
            if needed < std::mem::size_of::<AudioBufferList>() {
                return None;
            }
            // 8-byte aligned storage for the variable-length list.
            let mut storage = vec![0u64; needed.div_ceil(8)];
            let list = storage.as_mut_ptr().cast::<AudioBufferList>();
            let mut block: *mut CMBlockBuffer = ptr::null_mut();
            let status = sb.audio_buffer_list_with_retained_block_buffer(
                ptr::null_mut(),
                list,
                needed,
                None,
                None,
                // kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment
                1,
                &mut block,
            );
            // The retained block buffer is released when this drops.
            let _block = NonNull::new(block).map(|b| CFRetained::from_raw(b));
            if status != 0 {
                return None;
            }

            let count = (*list).mNumberBuffers as usize;
            let buffers = std::slice::from_raw_parts((*list).mBuffers.as_ptr(), count);
            let mut converted: Vec<(Vec<f32>, usize)> = Vec::with_capacity(count);
            for b in buffers {
                if b.mData.is_null() {
                    continue;
                }
                let bytes = b.mDataByteSize as usize;
                let samples = match kind {
                    SampleKind::F32 => {
                        let n = bytes / 4;
                        let mut v = vec![0f32; n];
                        ptr::copy_nonoverlapping(b.mData.cast::<u8>(), v.as_mut_ptr().cast::<u8>(), n * 4);
                        v
                    }
                    SampleKind::I16 => {
                        let n = bytes / 2;
                        let mut v = vec![0i16; n];
                        ptr::copy_nonoverlapping(b.mData.cast::<u8>(), v.as_mut_ptr().cast::<u8>(), n * 2);
                        f32_from_i16(&v)
                    }
                };
                converted.push((samples, b.mNumberChannels as usize));
            }
            let views: Vec<PcmBuffer<'_>> =
                converted.iter().map(|(s, c)| PcmBuffer { samples: s, channels: *c }).collect();
            let stereo = to_stereo(&views);
            (!stereo.is_empty()).then_some(stereo)
        }
    }

    /// The pieces of a running stream, kept alive together.
    struct Running {
        stream: Retained<SCStream>,
        output: Retained<AudioOutput>,
        _queue: dispatch2::DispatchRetained<DispatchQueue>,
    }

    /// A value from an SCK completion block, moved to the waiting thread. The objects are used only by that thread after
    /// the block returned.
    struct Handoff<T>(T);
    // SAFETY: see the type's doc: the block hands the value over and never touches it again.
    unsafe impl<T> Send for Handoff<T> {}

    fn error_parts(e: *mut NSError) -> Option<(i64, String)> {
        // SAFETY: SCK passes either null or a valid NSError for the duration of the block.
        unsafe { e.as_ref() }.map(|e| (e.code() as i64, e.localizedDescription().to_string()))
    }

    /// The first display (the main one: SCK lists it first) of the shareable content.
    fn main_display() -> Result<Retained<SCDisplay>> {
        let (tx, rx) = mpsc::channel::<std::result::Result<Handoff<Option<Retained<SCDisplay>>>, (i64, String)>>();
        let block = RcBlock::new(move |content: *mut SCShareableContent, err: *mut NSError| {
            let msg = match error_parts(err) {
                Some(e) => Err(e),
                None => {
                    // SAFETY: with no error SCK passes a valid SCShareableContent for the block's duration.
                    let display = unsafe { content.as_ref() }.and_then(|c| unsafe { c.displays() }.firstObject());
                    Ok(Handoff(display))
                }
            };
            let _ = tx.send(msg);
        });
        // SAFETY: the block lives until the call returns and SCK copies it for the asynchronous completion.
        unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&block) };
        match rx.recv_timeout(CALL_TIMEOUT) {
            Ok(Ok(Handoff(Some(d)))) => Ok(d),
            Ok(Ok(Handoff(None))) => Err(failure(ErrorKind::Unavailable, "SCShareableContent.displays", "no display")),
            Ok(Err((code, detail))) => Err(sck_error("SCShareableContent.getShareableContent", code, detail)),
            Err(RecvTimeoutError::Timeout) => {
                Err(failure(ErrorKind::Timeout, "SCShareableContent.getShareableContent", "no answer"))
            }
            Err(RecvTimeoutError::Disconnected) => {
                Err(failure(ErrorKind::Os, "SCShareableContent.getShareableContent", "completion dropped"))
            }
        }
    }

    fn run_completion(op: &'static str, call: impl FnOnce(&block2::DynBlock<dyn Fn(*mut NSError)>)) -> Result<()> {
        let (tx, rx) = mpsc::channel::<Option<(i64, String)>>();
        let block = RcBlock::new(move |err: *mut NSError| {
            let _ = tx.send(error_parts(err));
        });
        call(&block);
        match rx.recv_timeout(CALL_TIMEOUT) {
            Ok(None) => Ok(()),
            Ok(Some((code, detail))) => Err(sck_error(op, code, detail)),
            Err(RecvTimeoutError::Timeout) => Err(failure(ErrorKind::Timeout, op, "no answer")),
            Err(RecvTimeoutError::Disconnected) => Err(failure(ErrorKind::Os, op, "completion dropped")),
        }
    }

    fn open_stream(shared: &Arc<Shared>) -> Result<Running> {
        let display = main_display()?;
        // SAFETY: plain construction and configuration of SCK objects on live references; the stream keeps the filter
        // and configuration it needs. The output is added for the audio type only, on a serial queue.
        let running = unsafe {
            let no_windows = NSArray::<SCWindow>::new();
            let filter =
                SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), &display, &no_windows);
            let config = SCStreamConfiguration::new();
            config.setCapturesAudio(true);
            config.setSampleRate(48_000);
            config.setChannelCount(2);
            config.setExcludesCurrentProcessAudio(true);
            // SCStream insists on a video side: the smallest and slowest one.
            config.setWidth(2);
            config.setHeight(2);
            config.setMinimumFrameInterval(CMTime::new(1, 1));
            config.setShowsCursor(false);

            let output = AudioOutput::new(Arc::clone(shared));
            let delegate = ProtocolObject::<dyn SCStreamDelegate>::from_ref(&*output);
            let stream =
                SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &config, Some(delegate));
            let queue = DispatchQueue::new("swapdesk.capture.audio", DispatchQueueAttr::SERIAL);
            let sink = ProtocolObject::<dyn SCStreamOutput>::from_ref(&*output);
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(sink, SCStreamOutputType::Audio, Some(&queue))
                .map_err(|e| {
                    sck_error("SCStream.addStreamOutput", e.code() as i64, e.localizedDescription().to_string())
                })?;
            Running { stream, output, _queue: queue }
        };
        // SAFETY: the block passed lives for the call; SCK copies it.
        run_completion("SCStream.startCapture", |b| unsafe {
            running.stream.startCaptureWithCompletionHandler(Some(b))
        })?;
        Ok(running)
    }

    /// `AudioSource` for the Mac host: system audio over ScreenCaptureKit. Needs the Screen Recording grant.
    pub struct MacAudio {
        shared: Arc<Shared>,
        running: Option<Running>,
    }

    impl MacAudio {
        pub fn new(clock: Arc<dyn Clock>) -> Result<Self> {
            let shared = Arc::new(Shared {
                clock,
                state: Mutex::new(State { ring: BlockRing::new(RING_VALUES), error: None, running: false }),
                wake: Condvar::new(),
            });
            Ok(Self { shared, running: None })
        }

        /// Values dropped so far because the ring was full (the reader fell behind by more than 200 ms).
        pub fn dropped_values(&self) -> u64 {
            lock(&self.shared.state).ring.dropped_values()
        }
    }

    impl Drop for MacAudio {
        fn drop(&mut self) {
            self.stop();
        }
    }

    impl AudioSource for MacAudio {
        /// `exclude_pid` cannot be honoured by ScreenCaptureKit and is ignored: the app's own audio is always excluded
        /// (`excludesCurrentProcessAudio`), see the module comment.
        fn start(&mut self, _exclude_pid: Option<u32>) -> Result<()> {
            self.stop();
            if !CGPreflightScreenCaptureAccess() {
                return Err(failure(
                    ErrorKind::PermissionDenied(OsGrant::ScreenRecording),
                    "CGPreflightScreenCaptureAccess",
                    "Screen Recording is not granted to this app",
                ));
            }
            {
                let mut st = lock(&self.shared.state);
                st.ring.clear();
                st.error = None;
                st.running = true;
            }
            match open_stream(&self.shared) {
                Ok(r) => {
                    self.running = Some(r);
                    Ok(())
                }
                Err(e) => {
                    lock(&self.shared.state).running = false;
                    Err(e)
                }
            }
        }

        fn next_block(&mut self, timeout: Duration) -> Result<Option<AudioBlock>> {
            let deadline = Instant::now() + timeout;
            let mut st = lock(&self.shared.state);
            loop {
                if let Some((at, samples)) = st.ring.pop_block() {
                    // The clock's time of the block's first sample.
                    return Ok(Some(AudioBlock { captured_at: MonoMicros(at), samples }));
                }
                if let Some(e) = st.error.take() {
                    st.running = false;
                    return Err(e);
                }
                if !st.running {
                    return Err(failure(ErrorKind::Closed, "MacAudio.next_block", "not started"));
                }
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Ok(None);
                }
                st = self.shared.wake.wait_timeout(st, left).unwrap_or_else(|e| e.into_inner()).0;
            }
        }

        fn stop(&mut self) {
            lock(&self.shared.state).running = false;
            self.shared.wake.notify_all();
            if let Some(r) = self.running.take() {
                // A failed stop (the stream already ended) leaves nothing to undo.
                // SAFETY: the block passed lives for the call; SCK copies it.
                let _ = run_completion("SCStream.stopCapture", |b| unsafe {
                    r.stream.stopCaptureWithCompletionHandler(Some(b))
                });
                // SAFETY: the output was added by `open_stream` for this type.
                let _ = unsafe {
                    r.stream.removeStreamOutput_type_error(
                        ProtocolObject::<dyn SCStreamOutput>::from_ref(&*r.output),
                        SCStreamOutputType::Audio,
                    )
                };
            }
            lock(&self.shared.state).ring.clear();
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        struct TestClock(Instant);
        impl Clock for TestClock {
            fn now(&self) -> MonoMicros {
                MonoMicros(self.0.elapsed().as_micros() as u64)
            }
        }

        fn audio() -> MacAudio {
            MacAudio::new(Arc::new(TestClock(Instant::now()))).expect("new")
        }

        #[test]
        fn next_block_before_start_is_closed() {
            let mut a = audio();
            let e = a.next_block(Duration::from_millis(10)).unwrap_err();
            assert_eq!(e.kind, ErrorKind::Closed);
        }

        /// Over SSH nothing holds the grant: `start` must say so and not hang or crash. On a session that has the grant
        /// the test has nothing to check and returns.
        #[test]
        fn start_without_grant_is_permission_denied() {
            if CGPreflightScreenCaptureAccess() {
                eprintln!("Screen Recording is granted here: nothing to check");
                return;
            }
            let mut a = audio();
            let e = a.start(None).unwrap_err();
            assert_eq!(e.kind, ErrorKind::PermissionDenied(OsGrant::ScreenRecording), "{e}");
            // The source stays usable and closed.
            assert_eq!(a.next_block(Duration::from_millis(5)).unwrap_err().kind, ErrorKind::Closed);
        }

        #[test]
        #[ignore = "needs Screen Recording: run from a session that holds the grant (Terminal at the Mac)"]
        fn reads_one_second_of_blocks() {
            let mut a = audio();
            a.start(None).expect("start");
            let begin = Instant::now();
            let mut blocks = Vec::new();
            while begin.elapsed() < Duration::from_secs(1) {
                if let Some(b) = a.next_block(Duration::from_millis(50)).expect("next_block") {
                    blocks.push(b);
                }
            }
            a.stop();
            eprintln!("{} blocks, dropped values {}", blocks.len(), a.dropped_values());
            assert!(blocks.iter().all(|b| b.samples.len() == super::super::BLOCK_VALUES));
            // 1 s is 100 blocks; the first buffers take a moment to start and the last ones may be in flight.
            assert!((70..=130).contains(&blocks.len()), "{} blocks", blocks.len());
            for w in blocks.windows(2) {
                let step = w[1].captured_at.0 as i64 - w[0].captured_at.0 as i64;
                assert!((0..=50_000).contains(&step), "block step {step} us");
            }
            // The source can start again.
            a.start(None).expect("restart");
            assert!(a.next_block(Duration::from_secs(1)).expect("block").is_some());
            a.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(from: usize, n: usize) -> Vec<f32> {
        (from..from + n).map(|v| v as f32).collect()
    }

    #[test]
    fn planar_f32_becomes_interleaved() {
        let l = [1.0, 2.0, 3.0];
        let r = [-1.0, -2.0, -3.0];
        let out = to_stereo(&[PcmBuffer { samples: &l, channels: 1 }, PcmBuffer { samples: &r, channels: 1 }]);
        assert_eq!(out, vec![1.0, -1.0, 2.0, -2.0, 3.0, -3.0]);
    }

    #[test]
    fn planar_i16_becomes_interleaved_floats() {
        let l = f32_from_i16(&[16_384, -32_768]);
        let r = f32_from_i16(&[0, 32_767]);
        let out = to_stereo(&[PcmBuffer { samples: &l, channels: 1 }, PcmBuffer { samples: &r, channels: 1 }]);
        assert_eq!(out.len(), 4);
        assert_eq!(out[0], 0.5);
        assert_eq!(out[1], 0.0);
        assert_eq!(out[2], -1.0);
        assert!((out[3] - 32_767.0 / 32_768.0).abs() < 1e-9);
    }

    #[test]
    fn interleaved_stereo_passes_through() {
        let s = [0.1, 0.2, 0.3, 0.4];
        assert_eq!(to_stereo(&[PcmBuffer { samples: &s, channels: 2 }]), s.to_vec());
    }

    #[test]
    fn wider_layouts_keep_the_first_two_channels() {
        let s = [1.0, 2.0, 9.0, 3.0, 4.0, 9.0];
        assert_eq!(to_stereo(&[PcmBuffer { samples: &s, channels: 3 }]), vec![1.0, 2.0, 3.0, 4.0]);
        let a = [1.0, 2.0];
        let b = [5.0, 6.0];
        let c = [9.0, 9.0];
        let planes = [
            PcmBuffer { samples: &a, channels: 1 },
            PcmBuffer { samples: &b, channels: 1 },
            PcmBuffer { samples: &c, channels: 1 },
        ];
        assert_eq!(to_stereo(&planes), vec![1.0, 5.0, 2.0, 6.0]);
    }

    #[test]
    fn mono_is_duplicated() {
        let m = [0.25, 0.5];
        assert_eq!(to_stereo(&[PcmBuffer { samples: &m, channels: 1 }]), vec![0.25, 0.25, 0.5, 0.5]);
    }

    #[test]
    fn uneven_planes_keep_whole_frames_and_nothing_gives_empty() {
        let l = [1.0, 2.0, 3.0];
        let r = [4.0, 5.0];
        let out = to_stereo(&[PcmBuffer { samples: &l, channels: 1 }, PcmBuffer { samples: &r, channels: 1 }]);
        assert_eq!(out, vec![1.0, 4.0, 2.0, 5.0]);
        assert!(to_stereo(&[]).is_empty());
        assert!(to_stereo(&[PcmBuffer { samples: &l, channels: 0 }]).is_empty());
    }

    #[test]
    fn sample_kinds() {
        assert_eq!(sample_kind(FORMAT_LINEAR_PCM, FLAG_FLOAT | 8 | (1 << 5), 32), Some(SampleKind::F32));
        assert_eq!(sample_kind(FORMAT_LINEAR_PCM, FLAG_SIGNED_INT | 8, 16), Some(SampleKind::I16));
        assert_eq!(sample_kind(FORMAT_LINEAR_PCM, FLAG_FLOAT | FLAG_BIG_ENDIAN, 32), None);
        assert_eq!(sample_kind(FORMAT_LINEAR_PCM, FLAG_SIGNED_INT, 24), None);
        assert_eq!(sample_kind(0x6161_6320, FLAG_FLOAT, 32), None);
    }

    #[test]
    fn framing_across_uneven_chunks_keeps_order() {
        let mut ring = BlockRing::new(RING_VALUES);
        let mut next = 0usize;
        let mut blocks: Vec<Vec<f32>> = Vec::new();
        for n in [1000usize, 500, 460, 2, 4000, 6] {
            ring.push(&ramp(next, n), 1_000_000);
            next += n;
            while let Some((_, b)) = ring.pop_block() {
                blocks.push(b);
            }
        }
        assert_eq!(blocks.len(), next / BLOCK_VALUES);
        assert!(blocks.iter().all(|b| b.len() == BLOCK_VALUES));
        let flat: Vec<f32> = blocks.concat();
        assert_eq!(flat, ramp(0, flat.len()));
        assert_eq!(ring.len(), next - flat.len());
        assert_eq!(ring.dropped_values(), 0);
    }

    #[test]
    fn ring_drops_the_oldest_when_full() {
        let mut ring = BlockRing::new(RING_VALUES);
        let mut pushed = 0usize;
        for n in [7000usize, 7000, 7000, 4000] {
            ring.push(&ramp(pushed, n), 5_000_000);
            pushed += n;
            assert!(ring.len() <= RING_VALUES);
        }
        assert_eq!(ring.len(), RING_VALUES);
        assert_eq!(ring.dropped_values() as usize, pushed - RING_VALUES);
        // What stays is the newest audio, in order.
        let (_, first) = ring.pop_block().expect("block");
        assert_eq!(first[0] as usize, pushed - RING_VALUES);
        assert_eq!(first, ramp(pushed - RING_VALUES, BLOCK_VALUES));
    }

    #[test]
    fn one_chunk_larger_than_the_ring_keeps_its_tail() {
        let mut ring = BlockRing::new(RING_VALUES);
        ring.push(&ramp(0, RING_VALUES + 1_000), 1);
        assert_eq!(ring.len(), RING_VALUES);
        assert_eq!(ring.dropped_values(), 1_000);
        assert_eq!(ring.pop_block().expect("block").1[0], 1_000.0);
    }

    #[test]
    fn odd_trailing_value_is_ignored_and_frames_stay_whole() {
        let mut ring = BlockRing::new(RING_VALUES);
        ring.push(&ramp(0, 7), 1);
        assert_eq!(ring.len(), 6);
    }

    #[test]
    fn block_time_is_arrival_minus_the_audio_queued_behind() {
        let mut ring = BlockRing::new(RING_VALUES);
        // 2000 values = 1000 frames = 20.833 ms, arrived at t = 100 ms.
        ring.push(&ramp(0, 2_000), 100_000);
        let (t0, _) = ring.pop_block().expect("first");
        let (t1, _) = ring.pop_block().expect("second");
        assert_eq!(t0, 100_000 - 20_833);
        assert_eq!(t1 - t0, 10_000);
        assert!(ring.pop_block().is_none());
    }

    #[test]
    fn stream_stop_codes_map_to_kinds() {
        assert_eq!(kind_for_stream_code(-3821), ErrorKind::AccessLost);
        assert_eq!(kind_for_stream_code(-3817), ErrorKind::Closed);
        assert_eq!(kind_for_stream_code(-3801), ErrorKind::PermissionDenied(OsGrant::ScreenRecording));
        assert_eq!(kind_for_stream_code(-3814), ErrorKind::Unavailable);
        assert_eq!(kind_for_stream_code(-3811), ErrorKind::Os);
    }
}
