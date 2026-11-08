//! Small Media Foundation helpers: media types, samples, `ICodecAPI` values and output collection.

use std::mem::ManuallyDrop;

use swapdesk_platform::{ErrorKind, Result};
use windows::Win32::Media::MediaFoundation::{
    ICodecAPI, IMFAttributes, IMFMediaBuffer, IMFMediaType, IMFSample, IMFTransform, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_E_TRANSFORM_STREAM_CHANGE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFMediaType_Video, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFVideoInterlace_Progressive,
};
use windows::Win32::System::Variant::VARIANT;
use windows::core::GUID;

use crate::sys::{Ctx, err};

pub(crate) fn pack(a: u32, b: u32) -> u64 {
    u64::from(a) << 32 | u64::from(b)
}

pub(crate) fn unpack(v: u64) -> (u32, u32) {
    ((v >> 32) as u32, v as u32)
}

/// A video media type with the attributes both ends of the encoder and the decoder share.
pub(crate) fn video_type(subtype: &GUID, width: u32, height: u32, fps: u32) -> Result<IMFMediaType> {
    // SAFETY: attribute writes on a fresh media type.
    unsafe {
        let t = MFCreateMediaType().ctx("MFCreateMediaType")?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).ctx("MF_MT_MAJOR_TYPE")?;
        t.SetGUID(&MF_MT_SUBTYPE, subtype).ctx("MF_MT_SUBTYPE")?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).ctx("MF_MT_INTERLACE_MODE")?;
        if width > 0 && height > 0 {
            t.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height)).ctx("MF_MT_FRAME_SIZE")?;
        }
        if fps > 0 {
            t.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1)).ctx("MF_MT_FRAME_RATE")?;
        }
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1)).ctx("MF_MT_PIXEL_ASPECT_RATIO")?;
        Ok(t)
    }
}

/// Sets a `u32` value through `ICodecAPI`.
pub(crate) fn set_u32(codec: &ICodecAPI, key: &GUID, value: u32) -> windows::core::Result<()> {
    let v = VARIANT::from(value);
    // SAFETY: the VARIANT lives across the call.
    unsafe { codec.SetValue(key, &v) }
}

/// Sets a boolean value through `ICodecAPI`.
pub(crate) fn set_bool(codec: &ICodecAPI, key: &GUID, value: bool) -> windows::core::Result<()> {
    let v = VARIANT::from(value);
    // SAFETY: the VARIANT lives across the call.
    unsafe { codec.SetValue(key, &v) }
}

/// A memory sample holding `data` (one contiguous buffer).
pub(crate) fn memory_sample(data: &[u8]) -> Result<IMFSample> {
    // SAFETY: the buffer is locked, written within its max length and unlocked.
    unsafe {
        let buffer = MFCreateMemoryBuffer(data.len() as u32).ctx("MFCreateMemoryBuffer")?;
        let mut ptr = std::ptr::null_mut();
        buffer.Lock(&mut ptr, None, None).ctx("IMFMediaBuffer::Lock")?;
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
        buffer.Unlock().ctx("IMFMediaBuffer::Unlock")?;
        buffer.SetCurrentLength(data.len() as u32).ctx("SetCurrentLength")?;
        let sample = MFCreateSample().ctx("MFCreateSample")?;
        sample.AddBuffer(&buffer).ctx("AddBuffer")?;
        Ok(sample)
    }
}

/// An empty memory sample with room for `capacity` bytes (output buffers of software MFTs).
pub(crate) fn empty_sample(capacity: u32) -> Result<IMFSample> {
    // SAFETY: plain object creation.
    unsafe {
        let buffer = MFCreateMemoryBuffer(capacity).ctx("MFCreateMemoryBuffer")?;
        let sample = MFCreateSample().ctx("MFCreateSample")?;
        sample.AddBuffer(&buffer).ctx("AddBuffer")?;
        Ok(sample)
    }
}

/// Copies the bytes of a sample's buffers into one vector.
pub(crate) fn sample_bytes(sample: &IMFSample) -> Result<Vec<u8>> {
    // SAFETY: the contiguous buffer is locked for the copy and unlocked after.
    unsafe {
        let buffer: IMFMediaBuffer = sample.ConvertToContiguousBuffer().ctx("ConvertToContiguousBuffer")?;
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        buffer.Lock(&mut ptr, None, Some(&mut len)).ctx("IMFMediaBuffer::Lock")?;
        let out = std::slice::from_raw_parts(ptr, len as usize).to_vec();
        buffer.Unlock().ctx("IMFMediaBuffer::Unlock")?;
        Ok(out)
    }
}

/// What one `ProcessOutput` call gave.
pub(crate) enum Output {
    Sample(IMFSample),
    NeedMoreInput,
    /// The output type changed: the caller renegotiates and tries again.
    StreamChange,
}

/// One `ProcessOutput`. `provide` is the buffer to hand a transform that does not allocate its own samples.
pub(crate) fn process_output(transform: &IMFTransform, provide: Option<IMFSample>) -> Result<Output> {
    let mut buffer = MFT_OUTPUT_DATA_BUFFER {
        dwStreamID: 0,
        pSample: ManuallyDrop::new(provide),
        dwStatus: 0,
        pEvents: ManuallyDrop::new(None),
    };
    let mut status = 0u32;
    // SAFETY: one output buffer, valid for the call; the sample and events it returns are taken out exactly once.
    let result = unsafe { transform.ProcessOutput(0, std::slice::from_mut(&mut buffer), &mut status) };
    // SAFETY: both fields were initialised above and are taken once.
    let (sample, events) =
        unsafe { (ManuallyDrop::take(&mut buffer.pSample), ManuallyDrop::take(&mut buffer.pEvents)) };
    drop(events);
    match result {
        Ok(()) => match sample {
            Some(s) => Ok(Output::Sample(s)),
            None => Ok(Output::NeedMoreInput),
        },
        Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(Output::NeedMoreInput),
        Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => Ok(Output::StreamChange),
        Err(e) => Err(crate::sys::os_err("IMFTransform::ProcessOutput", &e)),
    }
}

/// How the transform delivers output samples.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OutputInfo {
    pub provides_samples: bool,
    pub buffer_size: u32,
}

pub(crate) fn output_info(transform: &IMFTransform) -> Result<OutputInfo> {
    // SAFETY: a getter on a live transform.
    let info = unsafe { transform.GetOutputStreamInfo(0) }.ctx("GetOutputStreamInfo")?;
    Ok(OutputInfo {
        provides_samples: info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0,
        buffer_size: info.cbSize,
    })
}

pub(crate) fn attrs_of(t: &IMFMediaType) -> Result<IMFAttributes> {
    use windows::core::Interface;
    t.cast().ctx("IMFMediaType as IMFAttributes").map_err(|e| err(ErrorKind::Os, e.op, e.detail))
}
