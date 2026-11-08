//! `AudioSink` over the default output audio unit (AUHAL specialisation `kAudioUnitSubType_DefaultOutput`).
//!
//! Why this path and not an `AudioQueue` or `AVAudioEngine`. Apple's SDK headers (read on the MacBook, macOS 27.2 SDK,
//! 2026-11-08), `AudioToolbox.framework/Headers/AUComponent.h`: "kAudioUnitSubType_DefaultOutput: A specialization of
//! AUHAL that is used to track the user's selection of the default device as set in the Sound Prefs." An output unit
//! is pull-driven: the device's I/O cycle calls our render callback for exactly the frames it needs, and the buffer of
//! that cycle is `kAudioDevicePropertyBufferFrameSize` frames (`CoreAudio.framework/Headers/AudioHardware.h`: "A UInt32
//! whose value indicates the number of frames in the IO buffers"), which can be asked down to a few milliseconds. An
//! `AudioQueue` is a queue of pre-filled buffers on top of the same unit (its header, `AudioQueue.h`, says a queue that
//! tracks the default device only *notifies* when it changes), so it adds at least one buffer of delay; `AVAudioEngine`
//! lives in `objc2-avf-audio`, which is not among the approved crates (D26). Audio unit property and function
//! signatures: https://docs.rs/objc2-audio-toolbox/0.3.2/objc2_audio_toolbox/ and
//! https://docs.rs/objc2-core-audio/0.3.2/objc2_core_audio/.
//!
//! Following the default device. The unit tracks the default output device by itself (header quote above). A property
//! listener on `kAudioObjectSystemObject` / `kAudioHardwarePropertyDefaultOutputDevice` counts the changes and makes the
//! next `write` ask the new device for the I/O buffer size again (a new device starts with its own).
//!
//! Threads. `write` runs on the caller's thread and only fills the lock-free ring of [`Playout`]; the render callback
//! runs on Core Audio's real-time thread and only reads it.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Arc;
use std::time::Duration;

use objc2_audio_toolbox::{
    AURenderCallbackStruct, AudioComponentDescription, AudioComponentFindNext, AudioComponentInstance,
    AudioComponentInstanceDispose, AudioComponentInstanceNew, AudioOutputUnitStart, AudioOutputUnitStop,
    AudioUnitInitialize, AudioUnitRenderActionFlags, AudioUnitSetProperty, AudioUnitUninitialize,
    kAudioUnitManufacturer_Apple, kAudioUnitProperty_SetRenderCallback, kAudioUnitProperty_StreamFormat,
    kAudioUnitScope_Input, kAudioUnitSubType_DefaultOutput, kAudioUnitType_Output,
};
use objc2_core_audio::{
    AudioObjectAddPropertyListener, AudioObjectGetPropertyData, AudioObjectPropertyAddress,
    AudioObjectRemovePropertyListener, AudioObjectSetPropertyData, kAudioDevicePropertyBufferFrameSize,
    kAudioDevicePropertyBufferFrameSizeRange, kAudioHardwarePropertyDefaultOutputDevice,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject,
};
use objc2_core_audio_types::{
    AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp, AudioValueRange, kAudioFormatFlagIsFloat,
    kAudioFormatFlagIsPacked, kAudioFormatLinearPCM,
};
use swapdesk_platform::{AudioBlock, AudioSink, ErrorKind, PlatformError, Result};

use crate::playout::{CHANNELS, Playout, PlayoutStats, SAMPLE_RATE, SinkConfig};

fn os_error(op: &'static str, status: i32) -> PlatformError {
    // 'nope'-style four character codes are easier to read than the number; both are kept.
    let bytes = status.to_be_bytes();
    let fourcc = if bytes.iter().all(|b| (0x20..0x7f).contains(b)) {
        format!(" '{}'", String::from_utf8_lossy(&bytes))
    } else {
        String::new()
    };
    PlatformError {
        kind: ErrorKind::Os,
        op,
        os_code: Some(i64::from(status)),
        detail: format!("OSStatus {status}{fourcc}"),
    }
}

fn check(op: &'static str, status: i32) -> Result<()> {
    if status == 0 { Ok(()) } else { Err(os_error(op, status)) }
}

fn closed(op: &'static str) -> PlatformError {
    PlatformError { kind: ErrorKind::Closed, op, os_code: None, detail: "not started".to_owned() }
}

fn default_device_address() -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: kAudioHardwarePropertyDefaultOutputDevice,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// The default output device's object id, or `None` when there is none (no output device, e.g. a headless Mac).
fn default_output_device() -> Option<u32> {
    let address = default_device_address();
    let mut device: u32 = 0;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: `address`, `size` and `device` outlive the call; `device` is 4 bytes, the size passed.
    let status = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject as u32,
            NonNull::from(&address),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut device).cast(),
        )
    };
    (status == 0 && device != 0).then_some(device)
}

/// Asks `device` for an I/O buffer of `frames` (clamped to the range it reports). Returns the size now in force, or
/// `None` when the device refused (the size it had stays).
fn set_buffer_frames(device: u32, frames: u32) -> Option<u32> {
    let range_address = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyBufferFrameSizeRange,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let size_address = AudioObjectPropertyAddress { mSelector: kAudioDevicePropertyBufferFrameSize, ..range_address };
    let mut range = AudioValueRange { mMinimum: 0.0, mMaximum: f64::from(u32::MAX) };
    let mut range_size = size_of::<AudioValueRange>() as u32;
    // SAFETY: all pointers refer to locals that outlive the call, with the sizes passed.
    let got_range = unsafe {
        AudioObjectGetPropertyData(
            device,
            NonNull::from(&range_address),
            0,
            std::ptr::null(),
            NonNull::from(&mut range_size),
            NonNull::from(&mut range).cast(),
        )
    };
    let wanted = if got_range == 0 { f64::from(frames).clamp(range.mMinimum, range.mMaximum) as u32 } else { frames };
    let mut value = wanted;
    // SAFETY: `value` is a UInt32 as the property requires.
    let set = unsafe {
        AudioObjectSetPropertyData(
            device,
            NonNull::from(&size_address),
            0,
            std::ptr::null(),
            size_of::<u32>() as u32,
            NonNull::from(&mut value).cast(),
        )
    };
    (set == 0).then_some(wanted)
}

/// The device I/O buffer in frames, when the device reports one.
fn buffer_frames(device: u32) -> Option<u32> {
    let address = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyBufferFrameSize,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut value = 0u32;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: locals outlive the call; `value` is 4 bytes, the size passed.
    let status = unsafe {
        AudioObjectGetPropertyData(
            device,
            NonNull::from(&address),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut value).cast(),
        )
    };
    (status == 0).then_some(value)
}

/// The render callback of the output unit (real-time thread): no allocation, no lock, no panic.
unsafe extern "C-unwind" fn render_callback(
    refcon: NonNull<c_void>,
    _flags: NonNull<AudioUnitRenderActionFlags>,
    _time: NonNull<AudioTimeStamp>,
    _bus: u32,
    _frames: u32,
    data: *mut AudioBufferList,
) -> i32 {
    // SAFETY: `refcon` is the pointer `start` made from an `Arc<Playout>` that stays alive until the unit is disposed.
    let playout = unsafe { &*refcon.as_ptr().cast::<Playout>() };
    // SAFETY: Core Audio passes a valid list (or null, handled) for the duration of the call.
    let Some(list) = (unsafe { data.as_mut() }) else {
        return 0;
    };
    let n = list.mNumberBuffers as usize;
    // SAFETY: `mBuffers` is a C flexible array holding `mNumberBuffers` entries.
    let buffers = unsafe { std::slice::from_raw_parts_mut(list.mBuffers.as_mut_ptr(), n) };
    for (i, buffer) in buffers.iter_mut().enumerate() {
        let values = buffer.mDataByteSize as usize / size_of::<f32>();
        if buffer.mData.is_null() || values == 0 {
            continue;
        }
        // SAFETY: the buffer holds `mDataByteSize` bytes of f32 and is ours to fill until the callback returns.
        let out = unsafe { std::slice::from_raw_parts_mut(buffer.mData.cast::<f32>(), values) };
        // The unit was given interleaved stereo, so there is one buffer; anything else is silenced rather than guessed.
        if i == 0 && n == 1 && buffer.mNumberChannels as usize == CHANNELS {
            playout.render(out);
        } else {
            out.fill(0.0);
        }
    }
    0
}

/// Counts changes of the default output device (a Core Audio notification thread).
unsafe extern "C-unwind" fn device_listener(
    _object: u32,
    _count: u32,
    _addresses: NonNull<AudioObjectPropertyAddress>,
    client: *mut c_void,
) -> i32 {
    // SAFETY: `client` is the `Arc<Playout>` pointer `start` registered; it is removed before that Arc is released.
    if let Some(playout) = unsafe { client.cast::<Playout>().as_ref() } {
        playout.note_device_change();
    }
    0
}

struct Running {
    unit: AudioComponentInstance,
    /// The strong count the callbacks borrow, given back in `drop`.
    client: *const Playout,
    reapplied_changes: usize,
    device_buffer_frames: Option<u32>,
}

impl Drop for Running {
    fn drop(&mut self) {
        let address = default_device_address();
        // SAFETY: undoes `start` in the reverse order. The listener is removed first so no notification touches the
        // `Playout`; stopping, uninitialising and disposing the unit ends the render callback before the last `Arc`
        // is released; each call tolerates a unit that was never started.
        unsafe {
            let _ = AudioObjectRemovePropertyListener(
                kAudioObjectSystemObject as u32,
                NonNull::from(&address),
                Some(device_listener),
                self.client.cast_mut().cast(),
            );
            let _ = AudioOutputUnitStop(self.unit);
            let _ = AudioUnitUninitialize(self.unit);
            let _ = AudioComponentInstanceDispose(self.unit);
            drop(Arc::from_raw(self.client));
        }
    }
}

/// The sink. Create it where it is used and `start` it; `write` 10 ms blocks as the jitter buffer releases them.
pub struct MacAudioSink {
    config: SinkConfig,
    playout: Arc<Playout>,
    running: Option<Running>,
}

impl MacAudioSink {
    pub fn new(config: SinkConfig) -> Self {
        Self { playout: Arc::new(Playout::new(&config)), config, running: None }
    }

    pub fn config(&self) -> &SinkConfig {
        &self.config
    }

    /// Linear gain, 0.0 (mute) to 1.0, ramped inside the next device cycle; takes effect while playing too.
    pub fn set_volume(&self, volume: f32) {
        self.playout.set_volume(volume);
    }

    pub fn volume(&self) -> f32 {
        self.playout.volume()
    }

    pub fn stats(&self) -> PlayoutStats {
        self.playout.stats()
    }

    /// Changes of the default output device seen since the sink was created.
    pub fn device_changes(&self) -> usize {
        self.playout.device_changes()
    }

    /// The I/O buffer of the default output device in frames, as read now.
    pub fn device_buffer_frames(&self) -> Option<u32> {
        default_output_device().and_then(buffer_frames)
    }

    /// The I/O buffer size the last request to the device left in force (`None`: not asked or refused).
    pub fn requested_buffer_frames(&self) -> Option<u32> {
        self.running.as_ref().and_then(|r| r.device_buffer_frames)
    }

    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    fn open(&mut self) -> Result<Running> {
        let description = AudioComponentDescription {
            componentType: kAudioUnitType_Output,
            componentSubType: kAudioUnitSubType_DefaultOutput,
            componentManufacturer: kAudioUnitManufacturer_Apple,
            componentFlags: 0,
            componentFlagsMask: 0,
        };
        // SAFETY: `description` outlives the call; a null first argument starts the search.
        let component = unsafe { AudioComponentFindNext(std::ptr::null_mut(), NonNull::from(&description)) };
        if component.is_null() {
            return Err(PlatformError {
                kind: ErrorKind::Unavailable,
                op: "AudioComponentFindNext(DefaultOutput)",
                os_code: None,
                detail: "no default output audio unit".to_owned(),
            });
        }
        let mut unit: AudioComponentInstance = std::ptr::null_mut();
        // SAFETY: `component` is valid; `unit` receives the instance.
        check("AudioComponentInstanceNew", unsafe { AudioComponentInstanceNew(component, NonNull::from(&mut unit)) })?;
        // From here on `Running` owns the unit: any early return disposes it.
        self.playout.reset();
        let client = Arc::into_raw(Arc::clone(&self.playout));
        let mut running =
            Running { unit, client, reapplied_changes: self.playout.device_changes(), device_buffer_frames: None };
        let format = AudioStreamBasicDescription {
            mSampleRate: f64::from(SAMPLE_RATE),
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
            mBytesPerPacket: (CHANNELS * size_of::<f32>()) as u32,
            mFramesPerPacket: 1,
            mBytesPerFrame: (CHANNELS * size_of::<f32>()) as u32,
            mChannelsPerFrame: CHANNELS as u32,
            mBitsPerChannel: 32,
            mReserved: 0,
        };
        // SAFETY: `format` is a valid ASBD of the size passed; the unit converts it to the device's own format.
        check("AudioUnitSetProperty(StreamFormat)", unsafe {
            AudioUnitSetProperty(
                unit,
                kAudioUnitProperty_StreamFormat,
                kAudioUnitScope_Input,
                0,
                std::ptr::from_ref(&format).cast(),
                size_of::<AudioStreamBasicDescription>() as u32,
            )
        })?;
        let callback =
            AURenderCallbackStruct { inputProc: Some(render_callback), inputProcRefCon: client.cast_mut().cast() };
        // SAFETY: the callback is a valid function and its refcon stays alive until `Running` is dropped.
        check("AudioUnitSetProperty(SetRenderCallback)", unsafe {
            AudioUnitSetProperty(
                unit,
                kAudioUnitProperty_SetRenderCallback,
                kAudioUnitScope_Input,
                0,
                std::ptr::from_ref(&callback).cast(),
                size_of::<AURenderCallbackStruct>() as u32,
            )
        })?;
        let address = default_device_address();
        // SAFETY: the listener is a valid function; its client pointer stays alive until `Running` is dropped, which
        // removes the listener first. A refusal only costs the change counter, so the status is not an error.
        let _ = unsafe {
            AudioObjectAddPropertyListener(
                kAudioObjectSystemObject as u32,
                NonNull::from(&address),
                Some(device_listener),
                client.cast_mut().cast(),
            )
        };
        running.device_buffer_frames = self.apply_buffer_size();
        // SAFETY: the unit is configured; both calls are plain state changes on it.
        check("AudioUnitInitialize", unsafe { AudioUnitInitialize(unit) })?;
        check("AudioOutputUnitStart", unsafe { AudioOutputUnitStart(unit) })?;
        Ok(running)
    }

    /// Asks the current default device for the configured I/O buffer (best effort).
    fn apply_buffer_size(&self) -> Option<u32> {
        let frames = self.config.device_buffer_frames?;
        set_buffer_frames(default_output_device()?, frames)
    }
}

impl AudioSink for MacAudioSink {
    fn start(&mut self) -> Result<()> {
        self.stop();
        let running = self.open()?;
        self.running = Some(running);
        Ok(())
    }

    fn write(&mut self, block: &AudioBlock) -> Result<()> {
        let Some(running) = self.running.as_mut() else {
            return Err(closed("MacAudioSink.write"));
        };
        let changes = self.playout.device_changes();
        if changes != running.reapplied_changes {
            running.reapplied_changes = changes;
            let frames = self.config.device_buffer_frames;
            running.device_buffer_frames =
                frames.and_then(|f| default_output_device().and_then(|d| set_buffer_frames(d, f)));
        }
        self.playout.push(&block.samples);
        Ok(())
    }

    fn queued(&self) -> Duration {
        if self.running.is_none() {
            return Duration::ZERO;
        }
        Duration::from_micros(self.playout.queued_frames() as u64 * 1_000_000 / u64::from(SAMPLE_RATE))
    }

    fn stop(&mut self) {
        self.running = None;
    }
}
